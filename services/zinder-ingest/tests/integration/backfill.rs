#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use eyre::{Result, eyre};
use serde_json::Value;
use tempfile::tempdir;
use zinder_core::{
    BlockHeight, BlockId, ChainTipMetadata, Network, ShieldedProtocol, SubtreeRootIndex,
};
use zinder_ingest::{BackfillConfig, BackfillOutcome, NodeSourceKind, backfill};
use zinder_query::{ArtifactKey, QueryError, WalletQuery, WalletQueryApi};
use zinder_source::{
    DEFAULT_MAX_JSON_RPC_RESPONSE_BYTES, NodeAuth, NodeCapabilities, NodeSource, NodeTarget,
    SourceBlock, SourceChainCheckpoint, SourceError, SourceSubtreeRoots, decode_display_block_hash,
};
use zinder_store::{
    ArtifactFamily, CURRENT_ARTIFACT_SCHEMA_VERSION, ChainEventHistoryRequest, ChainStoreOptions,
    PrimaryChainStore,
};

#[tokio::test]
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
        storage_path: storage_path.clone(),
        from_height: source_block.height,
        to_height: source_block.height,
        commit_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid batch size"))?,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };

    let BackfillOutcome::Committed(outcome) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!("expected checkpoint backfill to commit"));
    };

    assert_eq!(outcome.chain_epoch.network, Network::ZcashTestnet);
    assert_current_artifact_schema(outcome.chain_epoch);
    assert_eq!(outcome.chain_epoch.tip_height, source_block.height);
    assert_eq!(outcome.chain_epoch.finalized_height, source_block.height);
    assert_eq!(
        outcome.chain_epoch.tip_metadata,
        ChainTipMetadata::new(1, 0)
    );
    assert_eq!(
        fetched_heights
            .lock()
            .map_err(|_| eyre!("mutex poisoned"))?
            .as_slice(),
        [source_block.height]
    );

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

    let wallet_query = WalletQuery::new(store, ());
    let unavailable = match wallet_query.compact_block_at(checkpoint_height).await {
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

    let compact_block = wallet_query.compact_block_at(source_block.height).await?;
    assert_eq!(compact_block.chain_epoch.tip_height, source_block.height);
    assert_eq!(compact_block.compact_block.height, source_block.height);

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
        storage_path: tempdir.path().join("nonzero-checkpoint-backfill-store"),
        from_height: source_block.height,
        to_height: source_block.height,
        commit_batch_blocks: NonZeroU32::new(1).ok_or_else(|| eyre!("invalid batch size"))?,
        allow_near_tip_finalize: false,
        checkpoint: Some(checkpoint),
    };

    let BackfillOutcome::Committed(outcome) = backfill(&backfill_config, &source).await? else {
        return Err(eyre!("expected checkpoint backfill to commit"));
    };

    assert_eq!(outcome.chain_epoch.tip_metadata, expected_tip_metadata);

    Ok(())
}

struct FixtureCheckpointSource {
    block: SourceBlock,
    tip_height: BlockHeight,
    fetched_heights: Arc<Mutex<Vec<BlockHeight>>>,
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

    async fn fetch_block_by_height(&self, height: BlockHeight) -> Result<SourceBlock, SourceError> {
        self.fetched_heights
            .lock()
            .map_err(|_| SourceError::SourceProtocolMismatch {
                reason: "fixture source fetch history lock is poisoned",
            })?
            .push(height);

        if height != self.block.height {
            return Err(SourceError::BlockUnavailable {
                height,
                is_retryable: false,
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
