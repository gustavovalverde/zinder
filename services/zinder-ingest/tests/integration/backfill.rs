#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use eyre::{Result, eyre};
use parking_lot::Mutex;
use serde_json::Value;
use tempfile::tempdir;
use zinder_core::{
    BlockHeight, BlockId, ChainTipMetadata, Network, ShieldedProtocol, SubtreeRootIndex,
};
use zinder_derive::{BLOCK_SUMMARY_COLUMN_FAMILY, BlockSummaryConsumer, decode_stored_record};
use zinder_ingest::{
    BackfillConfig, NodeSourceKind, backfill, backfill_until_complete,
    catch_up_derive_store_to_canonical,
};
use zinder_query::{ArtifactKey, QueryError, WalletQuery, WalletQueryApi};
use zinder_runtime::{Readiness, ReadinessCause};
use zinder_source::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeCapabilities, NodeSource, NodeTarget,
    SourceBlock, SourceChainCheckpoint, SourceError, SourceSubtreeRoots, decode_display_block_hash,
};
use zinder_store::{
    ArtifactFamily, CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEventHistoryRequest, ChainStoreOptions,
    PrimaryChainStore,
};
use zinder_testkit::sample_regtest_upgrade_activations;

fn test_derive_store(storage_path: &Path) -> Result<zinder_derive::DeriveStore> {
    Ok(zinder_derive::DeriveStore::open(
        zinder_derive::DeriveStore::path_for_canonical(storage_path),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: &[],
            tuning: zinder_store::StorageTuning::for_local_tests(),
        },
    )?)
}

fn bundled_derive_store(storage_path: &Path) -> Result<zinder_derive::DeriveStore> {
    Ok(zinder_derive::DeriveStore::open(
        zinder_derive::DeriveStore::path_for_canonical(storage_path),
        zinder_derive::DeriveStoreOptions {
            sync_writes: false,
            consumer_column_families: zinder_derive::DeriveStore::bundled_consumer_column_families(
            ),
            tuning: zinder_store::StorageTuning::for_local_tests(),
        },
    )?)
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "scenario covers checkpoint bootstrap, backfill outcome assertions, and follow-up wallet queries; splitting into helpers obscures the end-to-end story"
)]
async fn backfill_bootstraps_empty_store_from_checkpoint() -> Result<()> {
    let source_block = fixture_source_block()?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = SourceChainCheckpoint::new(
        checkpoint_height,
        source_block.parent_hash,
        ChainTipMetadata::empty(),
    );
    let fetched_heights = Arc::new(Mutex::new(Vec::new()));
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: fetched_heights.clone(),
        pending_retryable_fetch_failures: Arc::new(Mutex::new(0)),
    };

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("checkpoint-backfill-store");
    let backfill_config = BackfillConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        storage_path: storage_path.clone(),
        from_height: source_block.height,
        to_height: source_block.height,
        commit_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid batch size"))?,
        max_transparent_prevout_store_lookups_per_batch: NonZeroU32::new(250_000)
            .ok_or_else(|| eyre!("invalid prevout budget"))?,
        fetch_concurrency: NonZeroU32::new(4).ok_or_else(|| eyre!("invalid fetch concurrency"))?,
        derive_concurrency: NonZeroU32::new(4)
            .ok_or_else(|| eyre!("invalid derive concurrency"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };

    let outcome = backfill(&backfill_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected checkpoint backfill to commit"))?;

    assert_eq!(outcome.chain_epoch.network, Network::ZcashTestnet);
    assert_current_artifact_schema(outcome.chain_epoch);
    assert_eq!(outcome.chain_epoch.tip_height, source_block.height);
    assert_eq!(outcome.chain_epoch.finalized_height, source_block.height);
    assert_eq!(
        outcome.chain_epoch.tip_metadata,
        ChainTipMetadata::new(1, 0)
    );
    assert_eq!(fetched_heights.lock().as_slice(), [source_block.height]);

    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashTestnet),
    )?;
    assert_eq!(
        store
            .chain_event_history(ChainEventHistoryRequest::with_default_limit(None))?
            .len(),
        2,
        "bootstrap epoch plus first backfilled block must both publish events"
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
    assert_eq!(compact_block.chain_epoch.tip_height, source_block.height);
    assert_eq!(compact_block.compact_block.height, source_block.height);

    Ok(())
}

#[tokio::test]
async fn derive_replay_catches_up_checkpoint_bootstrap_and_block_commit() -> Result<()> {
    let source_block = fixture_source_block()?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = SourceChainCheckpoint::new(
        checkpoint_height,
        source_block.parent_hash,
        ChainTipMetadata::empty(),
    );
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: Arc::new(Mutex::new(Vec::new())),
        pending_retryable_fetch_failures: Arc::new(Mutex::new(0)),
    };

    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("derive-replay-catchup-store");
    let backfill_config = BackfillConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        storage_path: storage_path.clone(),
        from_height: source_block.height,
        to_height: source_block.height,
        commit_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid batch size"))?,
        max_transparent_prevout_store_lookups_per_batch: NonZeroU32::new(250_000)
            .ok_or_else(|| eyre!("invalid prevout budget"))?,
        fetch_concurrency: NonZeroU32::new(4).ok_or_else(|| eyre!("invalid fetch concurrency"))?,
        derive_concurrency: NonZeroU32::new(4)
            .ok_or_else(|| eyre!("invalid derive concurrency"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashTestnet),
    )?;
    let derive_store_without_consumers = test_derive_store(&storage_path)?;
    let readiness = Readiness::default();

    backfill_until_complete(
        &backfill_config,
        &source,
        &store,
        &derive_store_without_consumers,
        &readiness,
    )
    .await?
    .ok_or_else(|| eyre!("expected backfill to commit"))?;
    drop(derive_store_without_consumers);

    let derive_store = bundled_derive_store(&storage_path)?;
    catch_up_derive_store_to_canonical(
        &store,
        &derive_store,
        NonZeroU32::new(2).ok_or_else(|| eyre!("invalid replay concurrency"))?,
    )
    .await?;

    assert_chain_event_cursors_advanced(&derive_store)?;
    assert_block_summary_materialized(&derive_store, source_block.height)?;

    Ok(())
}

fn assert_chain_event_cursors_advanced(derive_store: &zinder_derive::DeriveStore) -> Result<()> {
    for consumer_name in zinder_derive::DeriveStore::bundled_chain_event_consumer_names() {
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
async fn backfill_seeds_compact_metadata_from_nonzero_checkpoint() -> Result<()> {
    let checkpoint_tip_metadata = ChainTipMetadata::new(107_795, 0);
    let expected_tip_metadata = ChainTipMetadata::new(107_796, 0);
    let source_block = fixture_source_block()?.with_tree_state_payload_bytes(
        br#"{"sapling":{"commitments":{"size":107796}},"orchard":{"commitments":{"size":0}}}"#
            .to_vec(),
    );
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = SourceChainCheckpoint::new(
        checkpoint_height,
        source_block.parent_hash,
        checkpoint_tip_metadata,
    );
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: Arc::new(Mutex::new(Vec::new())),
        pending_retryable_fetch_failures: Arc::new(Mutex::new(0)),
    };
    let tempdir = tempdir()?;
    let backfill_config = BackfillConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_secs(30),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        storage_path: tempdir.path().join("nonzero-checkpoint-backfill-store"),
        from_height: source_block.height,
        to_height: source_block.height,
        commit_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid batch size"))?,
        max_transparent_prevout_store_lookups_per_batch: NonZeroU32::new(250_000)
            .ok_or_else(|| eyre!("invalid prevout budget"))?,
        fetch_concurrency: NonZeroU32::new(4).ok_or_else(|| eyre!("invalid fetch concurrency"))?,
        derive_concurrency: NonZeroU32::new(4)
            .ok_or_else(|| eyre!("invalid derive concurrency"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };

    let outcome = backfill(&backfill_config, &source)
        .await?
        .ok_or_else(|| eyre!("expected checkpoint backfill to commit"))?;

    assert_eq!(outcome.chain_epoch.tip_metadata, expected_tip_metadata);

    Ok(())
}

#[tokio::test]
async fn backfill_until_complete_resumes_after_retry_deadline() -> Result<()> {
    let source_block = fixture_source_block()?;
    let checkpoint_height = BlockHeight::new(source_block.height.value().saturating_sub(1));
    let checkpoint = SourceChainCheckpoint::new(
        checkpoint_height,
        source_block.parent_hash,
        ChainTipMetadata::empty(),
    );
    let pending_retryable_fetch_failures = Arc::new(Mutex::new(6));
    let fetched_heights = Arc::new(Mutex::new(Vec::new()));
    let source = FixtureCheckpointSource {
        block: source_block.clone(),
        tip_height: BlockHeight::new(source_block.height.value().saturating_add(200)),
        fetched_heights: fetched_heights.clone(),
        pending_retryable_fetch_failures: pending_retryable_fetch_failures.clone(),
    };
    let tempdir = tempdir()?;
    let storage_path = tempdir.path().join("recovering-backfill-store");
    let backfill_config = BackfillConfig {
        node: NodeTarget::new(
            Network::ZcashTestnet,
            "http://127.0.0.1:39232".to_owned(),
            NodeAuth::None,
            Duration::from_millis(1),
            DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES,
        ),
        node_source: NodeSourceKind::ZebraJsonRpc,
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        storage_path: storage_path.clone(),
        from_height: source_block.height,
        to_height: source_block.height,
        commit_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid batch size"))?,
        max_transparent_prevout_store_lookups_per_batch: NonZeroU32::new(250_000)
            .ok_or_else(|| eyre!("invalid prevout budget"))?,
        fetch_concurrency: NonZeroU32::new(4).ok_or_else(|| eyre!("invalid fetch concurrency"))?,
        derive_concurrency: NonZeroU32::new(4)
            .ok_or_else(|| eyre!("invalid derive concurrency"))?,
        flush_interval_epochs: NonZeroU32::new(5).ok_or_else(|| eyre!("invalid flush cadence"))?,
        upstream_tip_hint: None,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };
    let store = PrimaryChainStore::open(
        &storage_path,
        ChainStoreOptions::for_network(Network::ZcashTestnet),
    )?;
    let derive_store = test_derive_store(&storage_path)?;
    let readiness = Readiness::default();

    let outcome =
        backfill_until_complete(&backfill_config, &source, &store, &derive_store, &readiness)
            .await?
            .ok_or_else(|| eyre!("expected recovered backfill to commit"))?;

    assert_eq!(outcome.chain_epoch.tip_height, source_block.height);
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
        NodeCapabilities::default()
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
        decode_display_block_hash(json_string(&fixture, "hash")?)?
    );
    assert_eq!(
        source_block.parent_hash,
        decode_display_block_hash(json_string(&fixture, "previousblockhash")?)?
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
