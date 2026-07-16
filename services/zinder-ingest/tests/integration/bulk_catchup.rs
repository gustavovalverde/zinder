#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    num::{NonZeroU32, NonZeroU64},
    path::Path,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use eyre::{Result, eyre};
use parking_lot::Mutex;
use serde_json::Value;
use tempfile::tempdir;
use zinder_core::{
    BlockHeight, BlockId, ChainTipMetadata, CommitmentTreeCheckpoint, CommitmentTreeFrontier,
    CommitmentTreeFrontiers, Network, ShieldedProtocol, SubtreeRootIndex,
};
use zinder_derive::{
    BLOCK_SUMMARY_COLUMN_FAMILY, BlockSummaryConsumer, DeriveStoreError, ProjectionPreset,
    decode_stored_record,
};
use zinder_ingest::{
    BulkCatchupRunConfig, CanonicalPipelineLimits, DeriveReplayPolicy, IngestDeriveConfig,
    NodeSourceKind, catch_up_derive_store_to_canonical, run_bulk_catchup,
    run_bulk_catchup_until_complete,
};
use zinder_query::{ArtifactKey, QueryError, WalletQuery, WalletQueryApi};
use zinder_runtime::{Readiness, ReadinessCause};
use zinder_source::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeCapabilities, NodeCapability, NodeSource,
    NodeTarget, SourceBlock, SourceError, SourceSubtreeRoots, SourceTreeState,
    decode_rpc_block_hash,
};
use zinder_store::{
    ArtifactFamily, CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEventHistoryRequest, ChainStoreOptions,
    PrimaryChainStore, RawBlobRetention,
};
use zinder_testkit::{one_leaf_sapling_frontier, sample_regtest_upgrade_activations};

fn test_pipeline_limits() -> CanonicalPipelineLimits {
    CanonicalPipelineLimits {
        max_response_bytes: DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        source_segment_max_blocks: NonZeroU32::new(4).unwrap_or(NonZeroU32::MIN),
        source_segment_target_response_bytes: NonZeroU64::new(12 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        source_fetch_max_in_flight_requests: NonZeroU32::new(8).unwrap_or(NonZeroU32::MIN),
        source_fetch_max_in_flight_bytes: NonZeroU64::new(64 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
        block_prepare_concurrency: NonZeroU32::new(4).unwrap_or(NonZeroU32::MIN),
        block_prepare_memory_watermark_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .unwrap_or(NonZeroU64::MIN),
    }
}

fn bundled_derive_store(storage_path: &Path) -> Result<zinder_derive::DeriveStore> {
    Ok(zinder_derive::DeriveStore::open(
        zinder_derive::DeriveStore::path_for_canonical(storage_path),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumers: zinder_derive::DeriveStore::bundled_consumers(),
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        },
    )?)
}

fn test_all_blob_store_options() -> ChainStoreOptions {
    ChainStoreOptions {
        raw_blob_retention: RawBlobRetention::All,
        ..ChainStoreOptions::for_network(Network::ZcashTestnet)
    }
}

fn empty_checkpoint(block_id: BlockId) -> CommitmentTreeCheckpoint {
    CommitmentTreeCheckpoint::new(block_id, 0, CommitmentTreeFrontiers::default())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "scenario covers checkpoint bootstrap, bulk-catchup outcome assertions, and follow-up wallet queries; splitting into helpers obscures the end-to-end story"
)]
async fn bulk_catchup_bootstraps_empty_store_from_checkpoint() -> Result<()> {
    let source_block = fixture_source_block()?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = empty_checkpoint(BlockId::new(checkpoint_height, source_block.parent_hash));
    let fetched_heights = Arc::new(Mutex::new(Vec::new()));
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: fetched_heights.clone(),
        pending_retryable_fetch_failures: Arc::new(Mutex::new(0)),
    };

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("checkpoint-bulk-catchup-store");
    let bulk_catchup_config = BulkCatchupRunConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        storage_path: storage_path.clone(),
        reorg_window_blocks: 100,
        raw_blob_policy: zinder_ingest::RawBlobPolicy::All,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        from_height: source_block.height,
        to_height: source_block.height,
        canonical_batch_max_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid batch size"))?,
        canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid batch artifact bytes"))?,
        canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        )
        .ok_or_else(|| eyre!("invalid estimated write bytes"))?,
        canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )
        .ok_or_else(|| eyre!("invalid estimated write close floor"))?,
        pipeline_limits: test_pipeline_limits(),
        commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid commit reassembly bytes"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };

    let outcome = run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected checkpoint bulk catchup to commit"))?;

    assert_eq!(outcome.chain_epoch.network, Network::ZcashTestnet);
    assert_current_artifact_schema(outcome.chain_epoch);
    assert_eq!(outcome.chain_epoch.visible_tip_height, source_block.height);
    assert_eq!(outcome.chain_epoch.settled_tip_height, source_block.height);
    assert_eq!(
        outcome.chain_epoch.tip_metadata,
        ChainTipMetadata::new(1, 0, 0)
    );
    assert_eq!(fetched_heights.lock().as_slice(), [source_block.height]);

    let store = PrimaryChainStore::open(&storage_path, test_all_blob_store_options())?;
    assert_eq!(
        store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?
            .len(),
        2,
        "bootstrap epoch plus first bulk-caught-up block must both publish events"
    );

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let unavailable = match wallet_query.compact_block_at(checkpoint_height, None).await {
        Ok(response) => {
            return Err(eyre!(
                "expected checkpoint artifact unavailable, got {response:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(matches!(
        unavailable,
        QueryError::ArtifactUnavailable {
            family: ArtifactFamily::CompactBlock,
            key: ArtifactKey::BlockHeight(height),
        } if height == checkpoint_height
    ));

    let compact_block = wallet_query
        .compact_block_at(source_block.height, None)
        .await?;
    assert_eq!(
        compact_block.chain_epoch.visible_tip_height,
        source_block.height
    );
    assert_eq!(compact_block.compact_block.height, source_block.height);
    let tree_state = wallet_query
        .tree_state_at(source_block.height, None)
        .await?;
    assert_eq!(tree_state.height, source_block.height);
    assert_eq!(tree_state.block_hash, source_block.hash);

    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "scenario covers checkpoint bootstrap, run_bulk_catchup, derive replay, and materialized projection assertions end to end"
)]
async fn derive_replay_catches_up_checkpoint_bootstrap_and_block_commit() -> Result<()> {
    let source_block = fixture_source_block()?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = empty_checkpoint(BlockId::new(checkpoint_height, source_block.parent_hash));
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: Arc::new(Mutex::new(Vec::new())),
        pending_retryable_fetch_failures: Arc::new(Mutex::new(0)),
    };

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("derive-replay-catchup-store");
    let bulk_catchup_config = BulkCatchupRunConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        storage_path: storage_path.clone(),
        reorg_window_blocks: 100,
        raw_blob_policy: zinder_ingest::RawBlobPolicy::All,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        from_height: source_block.height,
        to_height: source_block.height,
        canonical_batch_max_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid batch size"))?,
        canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid batch artifact bytes"))?,
        canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        )
        .ok_or_else(|| eyre!("invalid estimated write bytes"))?,
        canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )
        .ok_or_else(|| eyre!("invalid estimated write close floor"))?,
        pipeline_limits: test_pipeline_limits(),
        commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid commit reassembly bytes"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };
    let store = PrimaryChainStore::open(&storage_path, test_all_blob_store_options())?;
    let readiness = Readiness::default();

    run_bulk_catchup_until_complete(&bulk_catchup_config, &source, &store, &readiness)
        .await?
        .ok_or_else(|| eyre!("expected bulk catchup to commit"))?;

    let replay = store
        .current_chain_epoch_reader()?
        .current_transparent_spend_replay_at_height(source_block.height)?
        .ok_or_else(|| eyre!("checkpoint block did not persist its transparent input set"))?;
    assert_eq!(replay.block_hash, source_block.hash);
    assert!(!replay.input_outpoints.is_empty());
    assert!(
        replay.spend_facts.is_empty(),
        "parents older than the checkpoint must remain explicitly unresolved"
    );

    let derive_store = bundled_derive_store(&storage_path)?;
    let derive_config = IngestDeriveConfig {
        replay_batch_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid replay batch blocks"))?,
        replay_policy: DeriveReplayPolicy::DEFAULT,
        memory_budget_bytes: None,
        memory_degrade_ratio: 0.85,
        memory_pause_ratio: 0.95,
        memory_resume_ratio: 0.75,
        min_replay_batch_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid minimum replay batch blocks"))?,
        startup_handoff_lag_blocks: 1_000,
    };
    catch_up_derive_store_to_canonical(&store, &derive_store, derive_config).await?;

    assert_chain_event_cursors_advanced(&derive_store)?;
    assert_block_summary_materialized(&derive_store, source_block.height)?;
    assert_paid_fee_live_tail_seeded(&derive_store, source_block.height)?;

    let wallet_derive_store = zinder_derive::DeriveStore::open_with_projection_preset(
        storage_path.join("wallet-derive"),
        ProjectionPreset::Wallet,
        zinder_derive::DeriveStoreOptions {
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            ..zinder_derive::DeriveStoreOptions::default()
        },
    )?;
    catch_up_derive_store_to_canonical(&store, &wallet_derive_store, derive_config).await?;
    for schema in ProjectionPreset::Wallet.consumer_schemas() {
        assert!(
            wallet_derive_store
                .get_chain_event_cursor(schema.name)?
                .is_some(),
            "wallet projection {} must advance through retained canonical events",
            schema.name.as_str()
        );
    }
    assert!(matches!(
        wallet_derive_store.consumer_column_family(BLOCK_SUMMARY_COLUMN_FAMILY),
        Err(DeriveStoreError::ConsumerColumnFamilyMissing { name })
            if name == BLOCK_SUMMARY_COLUMN_FAMILY
    ));

    Ok(())
}

fn assert_paid_fee_live_tail_seeded(
    derive_store: &zinder_derive::DeriveStore,
    block_height: BlockHeight,
) -> Result<()> {
    let tail = zinder_derive::PaidFeeDistributionConsumer::tail_coverage(derive_store)?
        .ok_or_else(|| eyre!("derive replay did not seed the paid-fee live tail"))?;
    assert_eq!(tail.boundary_height, block_height);
    assert_eq!(tail.complete_through_height, Some(block_height));
    Ok(())
}

fn assert_chain_event_cursors_advanced(derive_store: &zinder_derive::DeriveStore) -> Result<()> {
    for consumer_name in zinder_derive::DeriveStore::bundled_chain_event_consumer_names() {
        if *consumer_name == zinder_derive::TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME {
            assert!(
                derive_store
                    .get_chain_event_cursor(*consumer_name)?
                    .is_none(),
                "snapshot-owned ranking must not adopt a cursor before bootstrap activation"
            );
            continue;
        }
        assert!(
            derive_store
                .get_chain_event_cursor(*consumer_name)?
                .is_some(),
            "derive replay must advance {consumer_name:?} through the retained canonical events"
        );
    }
    Ok(())
}

fn assert_block_summary_materialized(
    derive_store: &zinder_derive::DeriveStore,
    block_height: BlockHeight,
) -> Result<()> {
    let record_bytes = derive_store
        .get_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &BlockSummaryConsumer::key_for_height(block_height),
        )?
        .ok_or_else(|| eyre!("derive replay did not materialize the block summary"))?;
    let block_summary_record = decode_stored_record(&record_bytes)?;
    let summary = block_summary_record
        .summary
        .ok_or_else(|| eyre!("block summary record did not carry a summary"))?;
    assert_eq!(summary.block_height, block_height.value());
    Ok(())
}

#[tokio::test]
async fn bulk_catchup_seeds_compact_metadata_from_valid_nonzero_checkpoint() -> Result<()> {
    let source_block = super::fixture_block::fixture_ironwood_source_block()
        .map_err(|error| eyre!(error.to_string()))?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = CommitmentTreeCheckpoint::new(
        BlockId::new(checkpoint_height, source_block.parent_hash),
        0,
        CommitmentTreeFrontiers::from_validated_parts(
            Some(one_leaf_sapling_frontier()?),
            Some(CommitmentTreeFrontier::empty(ShieldedProtocol::Orchard)),
            None,
        ),
    );
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: Arc::new(Mutex::new(Vec::new())),
        pending_retryable_fetch_failures: Arc::new(Mutex::new(0)),
    };
    let tempdir = tempdir()?;
    let bulk_catchup_config = BulkCatchupRunConfig {
        node: NodeTarget::new(
            Network::ZcashRegtest,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        storage_path: tempdir.path().join("nonzero-checkpoint-bulk-catchup-store"),
        reorg_window_blocks: 100,
        raw_blob_policy: zinder_ingest::RawBlobPolicy::All,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        from_height: source_block.height,
        to_height: source_block.height,
        canonical_batch_max_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid batch size"))?,
        canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid batch artifact bytes"))?,
        canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        )
        .ok_or_else(|| eyre!("invalid estimated write bytes"))?,
        canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )
        .ok_or_else(|| eyre!("invalid estimated write close floor"))?,
        pipeline_limits: test_pipeline_limits(),
        commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid commit reassembly bytes"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };

    let outcome = run_bulk_catchup(&bulk_catchup_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected checkpoint bulk catchup to commit"))?;

    assert_eq!(
        outcome.chain_epoch.tip_metadata,
        ChainTipMetadata::new(1, 0, 2)
    );
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "scenario keeps retry-deadline recovery, readiness, and fetch-count assertions in one observable bulk-catchup flow"
)]
async fn run_bulk_catchup_until_complete_resumes_after_retry_deadline() -> Result<()> {
    let source_block = fixture_source_block()?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = empty_checkpoint(BlockId::new(checkpoint_height, source_block.parent_hash));
    let pending_retryable_fetch_failures = Arc::new(Mutex::new(6));
    let fetched_heights = Arc::new(Mutex::new(Vec::new()));
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: fetched_heights.clone(),
        pending_retryable_fetch_failures: pending_retryable_fetch_failures.clone(),
    };
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("recovering-bulk-catchup-store");
    let bulk_catchup_config = BulkCatchupRunConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_millis(1),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        storage_path: storage_path.clone(),
        reorg_window_blocks: 100,
        raw_blob_policy: zinder_ingest::RawBlobPolicy::All,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        from_height: source_block.height,
        to_height: source_block.height,
        canonical_batch_max_blocks: NonZeroU32::new(1)
            .ok_or_else(|| eyre!("invalid batch size"))?,
        canonical_batch_max_artifact_bytes: NonZeroU64::new(512 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid batch artifact bytes"))?,
        canonical_batch_max_estimated_write_bytes: NonZeroU64::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MAX_ESTIMATED_WRITE_BYTES,
        )
        .ok_or_else(|| eyre!("invalid estimated write bytes"))?,
        canonical_batch_min_blocks_before_estimated_write_close: NonZeroU32::new(
            zinder_ingest::DEFAULT_CANONICAL_BATCH_MIN_BLOCKS_BEFORE_ESTIMATED_WRITE_CLOSE,
        )
        .ok_or_else(|| eyre!("invalid estimated write close floor"))?,
        pipeline_limits: test_pipeline_limits(),
        commit_reassembly_max_queued_artifact_bytes: NonZeroU64::new(128 * 1024 * 1024)
            .ok_or_else(|| eyre!("invalid commit reassembly bytes"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };
    let store = PrimaryChainStore::open(&storage_path, test_all_blob_store_options())?;
    let readiness = Readiness::default();

    let outcome =
        run_bulk_catchup_until_complete(&bulk_catchup_config, &source, &store, &readiness)
            .await?
            .ok_or_else(|| eyre!("expected recovered bulk catchup to commit"))?;

    assert_eq!(outcome.chain_epoch.visible_tip_height, source_block.height);
    assert_eq!(*pending_retryable_fetch_failures.lock(), 0);
    assert_eq!(fetched_heights.lock().len(), 7);
    let readiness_report = readiness.report();
    assert!(readiness_report.is_ready);
    assert_eq!(readiness_report.cause, ReadinessCause::Ready);
    assert_eq!(
        readiness_report.current_height,
        Some(source_block.height.value())
    );

    Ok(())
}

#[derive(Clone)]
struct FixtureCheckpointSource {
    block: SourceBlock,
    tip_height: BlockHeight,
    fetched_heights: Arc<Mutex<Vec<BlockHeight>>>,
    pending_retryable_fetch_failures: Arc<Mutex<u32>>,
}

fn assert_current_artifact_schema(chain_epoch: zinder_core::ChainEpoch) {
    assert_eq!(
        chain_epoch.artifact_schema_version,
        CURRENT_ARTIFACT_SCHEMA_VERSION
    );
}

#[async_trait]
impl NodeSource for FixtureCheckpointSource {
    fn capabilities(&self) -> NodeCapabilities {
        NodeCapabilities::new([NodeCapability::TreeState]).unwrap_or_default()
    }

    async fn fetch_block_at(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.fetched_heights.lock().push(height);

        let mut pending_retryable_fetch_failures = self.pending_retryable_fetch_failures.lock();
        if *pending_retryable_fetch_failures > 0 {
            *pending_retryable_fetch_failures = pending_retryable_fetch_failures.saturating_sub(1);
            return Err(SourceError::NodeUnavailable {
                reason: "fixture upstream outage".to_owned(),
            });
        }
        drop(pending_retryable_fetch_failures);

        if height != self.block.height {
            return Err(SourceError::BlockUnavailable {
                height,
                reason: "fixture source only serves the configured block".to_owned(),
            });
        }

        Ok(self.block.clone())
    }

    async fn tip_id(&self) -> Result<BlockId, SourceError> {
        Ok(BlockId::new(self.tip_height, self.block.hash))
    }

    async fn fetch_tree_state_for_block(
        &self,
        block_id: BlockId,
    ) -> Result<SourceTreeState, SourceError> {
        if block_id != BlockId::new(self.block.height, self.block.hash) {
            return Err(SourceError::BlockUnavailable {
                height: block_id.height,
                reason: "fixture source only serves the configured block tree state".to_owned(),
            });
        }
        let payload = format!(
            r#"{{"hash":"{}","height":{},"time":{},"sapling":{{"commitments":{{"size":1,"finalState":"000000"}}}},"orchard":{{"commitments":{{"size":0,"finalState":"111111"}}}}}}"#,
            hex::encode(block_id.hash.as_bytes()),
            block_id.height.value(),
            self.block.block_time_seconds
        );
        Ok(SourceTreeState::new(block_id, payload.into_bytes()))
    }

    async fn fetch_subtree_roots(
        &self,
        protocol: ShieldedProtocol,
        start_index: SubtreeRootIndex,
        _max_entries: NonZeroU32,
    ) -> Result<SourceSubtreeRoots, SourceError> {
        Ok(SourceSubtreeRoots::new(protocol, start_index, Vec::new()))
    }
}

fn fixture_source_block() -> Result<SourceBlock> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../fixtures/zcash-testnet-sapling-block-1842432.json"
    ))?;
    let raw_block_bytes = hex::decode(json_string(&fixture, "raw_block_hex")?)?;
    let height = BlockHeight::new(json_u32(&fixture, "height")?);
    let source_block =
        SourceBlock::from_raw_block_bytes(Network::ZcashTestnet, height, raw_block_bytes)?;

    assert_eq!(
        source_block.hash,
        decode_rpc_block_hash(json_string(&fixture, "hash")?)?
    );
    assert_eq!(
        source_block.parent_hash,
        decode_rpc_block_hash(json_string(&fixture, "previousblockhash")?)?
    );
    assert_eq!(source_block.block_time_seconds, json_u32(&fixture, "time")?);

    Ok(source_block)
}

fn json_string<'fixture>(
    fixture: &'fixture Value,
    field_name: &'static str,
) -> Result<&'fixture str> {
    fixture
        .get(field_name)
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("fixture is missing string field {field_name}"))
}

fn json_u32(fixture: &Value, field_name: &'static str) -> Result<u32> {
    let field_number = fixture
        .get(field_name)
        .and_then(Value::as_u64)
        .ok_or_else(|| eyre!("fixture is missing u32 field {field_name}"))?;
    Ok(u32::try_from(field_number)?)
}
