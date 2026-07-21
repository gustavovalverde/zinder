#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::eyre;
use prost::Message;
use zinder_core::wire::{encode_rpc_block_hash_hex, encode_zinder_native_chain_name};
use zinder_core::{
    BlockHeight, BlockHeightRange, BlockId, ChainEpoch, ChainTipMetadata, CompactBlockArtifact,
    CompactChainMetadata, ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex,
    SubtreeRootRange, TreeStateArtifact,
};
use zinder_proto::v1::wallet;
use zinder_query::{
    ArtifactKey, QueryError, WalletQuery, WalletQueryApi, WalletQueryOptions,
    latest_tree_state_checkpoint_response, subtree_roots_response, tree_state_at_response,
    visible_tip_block_response,
};
use zinder_source::{SourceError, SourceTreeState, TreeStateUpstream};
use zinder_store::{ArtifactFamily, ChainEpochArtifacts};
use zinder_testkit::{
    StoreFixture, encode_fixture_block_replay, sample_regtest_upgrade_activations,
};

use crate::common::{
    chain_epoch_artifacts_with_sapling_outputs, compact_block_with_tree_sizes,
    synthetic_chain_epoch,
};

#[derive(Clone)]
struct FixedTreeStateUpstream(SourceTreeState);

#[async_trait]
impl TreeStateUpstream for FixedTreeStateUpstream {
    async fn fetch_tree_state_for_block(
        &self,
        _block_id: BlockId,
    ) -> Result<SourceTreeState, SourceError> {
        Ok(self.0.clone())
    }
}

#[tokio::test]
async fn compact_block_range_reads_from_one_chain_epoch() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);
    let first_replay = encode_fixture_block_replay(&first_block, &[]);
    let second_replay = encode_fixture_block_replay(&second_block, &[]);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_replay],
        vec![first_compact_block.clone()],
    ))?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_replay],
        vec![second_compact_block.clone()],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let compact_block_range = wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await?;

    assert_eq!(compact_block_range.chain_epoch, second_epoch);
    assert_eq!(
        compact_block_range.compact_blocks,
        vec![first_compact_block, second_compact_block]
    );

    Ok(())
}

#[tokio::test]
async fn compact_block_range_serves_visible_blocks_above_settled_tip() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (mut second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);
    second_epoch.settled_tip_height = first_epoch.visible_tip_height;
    second_epoch.settled_tip_hash = first_epoch.visible_tip_hash;

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block.clone()],
        vec![encode_fixture_block_replay(&first_block, &[])],
        vec![first_compact_block],
    ))?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block.clone()],
        vec![encode_fixture_block_replay(&second_block, &[])],
        vec![second_compact_block.clone()],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(2), BlockHeight::new(2)),
            Some(second_epoch.id),
        )
        .await?;

    assert_eq!(response.chain_epoch, second_epoch);
    assert_eq!(response.compact_blocks, vec![second_compact_block]);
    Ok(())
}

#[tokio::test]
async fn tree_state_at_serves_visible_height_above_settled_tip_under_epoch_pin() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (mut second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);
    second_epoch.settled_tip_height = first_epoch.visible_tip_height;
    second_epoch.settled_tip_hash = first_epoch.visible_tip_hash;

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block.clone()],
        vec![encode_fixture_block_replay(&first_block, &[])],
        vec![first_compact_block],
    ))?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block.clone()],
        vec![encode_fixture_block_replay(&second_block, &[])],
        vec![second_compact_block],
    ))?;

    let block_id = BlockId::new(second_block.height, second_block.block_hash);
    let payload = br#"{"orchard":{"commitments":{"finalState":"aa"}}}"#.to_vec();
    let upstream = FixedTreeStateUpstream(SourceTreeState::new(
        block_id,
        u32::try_from(second_block.block_time)?,
        payload.clone(),
    ));
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_tree_state_upstream(Arc::new(upstream));

    let response = wallet_query
        .tree_state_at(second_block.height, Some(second_epoch.id))
        .await?;

    assert_eq!(response.chain_epoch, second_epoch);
    assert_eq!(response.height, second_block.height);
    assert_eq!(response.block_hash, second_block.block_hash);
    assert_eq!(response.payload_bytes, payload);
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "the test keeps the complete native compact-block wire shape in one assertion scope"
)]
async fn compact_block_range_chunk_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let replay = encode_fixture_block_replay(&block, &[]);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block.clone()],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let compact_block_range = wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(1)),
            None,
        )
        .await?;
    let response_compact_block = compact_block_range
        .compact_blocks
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing compact block"))?;
    let chunk = compact_block_range_chunk(compact_block_range.chain_epoch, &response_compact_block);
    let encoded_chunk = chunk.encode_to_vec();
    let decoded_chunk = wallet::CompactBlocksInRangeChunk::decode(encoded_chunk.as_slice())?;
    let response_chain_epoch = decoded_chunk
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| eyre!("missing chunk chain epoch"))?;
    let response_visible_tip = response_chain_epoch
        .visible_tip
        .as_ref()
        .ok_or_else(|| eyre!("missing chunk visible tip"))?;
    let response_settled_tip = response_chain_epoch
        .settled_tip
        .as_ref()
        .ok_or_else(|| eyre!("missing chunk settled tip"))?;
    let response_compact_block = decoded_chunk
        .compact_block
        .as_ref()
        .ok_or_else(|| eyre!("missing chunk compact block"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(
        response_chain_epoch.network_name,
        encode_zinder_native_chain_name(chain_epoch.network)
    );
    assert_eq!(
        response_visible_tip.height,
        chain_epoch.visible_tip_height.value()
    );
    assert_eq!(
        response_visible_tip.hash,
        encode_rpc_block_hash_hex(chain_epoch.visible_tip_hash)
    );
    assert_eq!(
        response_settled_tip.height,
        chain_epoch.settled_tip_height.value()
    );
    assert_eq!(
        response_settled_tip.hash,
        encode_rpc_block_hash_hex(chain_epoch.settled_tip_hash)
    );
    assert_eq!(
        response_chain_epoch.artifact_schema_version,
        u32::from(chain_epoch.artifact_schema_version.value())
    );
    assert_eq!(
        response_chain_epoch.created_at_millis,
        chain_epoch.created_at.value()
    );
    assert_eq!(
        response_compact_block,
        &zinder_proto::wire::compact_block_message(&compact_block)
    );

    Ok(())
}

#[tokio::test]
async fn visible_tip_block_response_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let replay = encode_fixture_block_replay(&block, &[]);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = visible_tip_block_response(&wallet_query, None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::VisibleTipBlockResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| eyre!("missing response chain epoch"))?;
    let visible_tip_block = decoded_response
        .visible_tip_block
        .as_ref()
        .ok_or_else(|| eyre!("missing visible-tip block identity"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(
        visible_tip_block.height,
        chain_epoch.visible_tip_height.value()
    );
    assert_eq!(
        visible_tip_block.block_hash,
        encode_rpc_block_hash_hex(chain_epoch.visible_tip_hash)
    );

    Ok(())
}

#[tokio::test]
async fn tree_state_checkpoint_response_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        BlockHeight::new(1),
        chain_epoch.visible_tip_hash,
        u32::try_from(block.block_time)?,
        b"tree-state-1".to_vec(),
    );
    let replay = encode_fixture_block_replay(&block, &[]);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![replay], vec![compact_block])
            .with_tree_states(vec![tree_state.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = tree_state_at_response(&wallet_query, BlockHeight::new(1), None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::TreeStateResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| eyre!("missing response chain epoch"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(decoded_response.height, tree_state.height.value());
    assert_eq!(
        decoded_response.block_hash,
        encode_rpc_block_hash_hex(tree_state.block_hash)
    );
    assert_eq!(decoded_response.payload_bytes, tree_state.payload_bytes);

    Ok(())
}

#[tokio::test]
async fn latest_tree_state_checkpoint_response_uses_tip_tree_state() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        BlockHeight::new(1),
        chain_epoch.visible_tip_hash,
        u32::try_from(block.block_time)?,
        b"tree-state-1".to_vec(),
    );
    let replay = encode_fixture_block_replay(&block, &[]);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![replay], vec![compact_block])
            .with_tree_states(vec![tree_state.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = latest_tree_state_checkpoint_response(&wallet_query, None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::TreeStateResponse::decode(encoded_response.as_slice())?;

    assert_eq!(decoded_response.height, tree_state.height.value());
    assert_eq!(
        decoded_response.block_hash,
        encode_rpc_block_hash_hex(tree_state.block_hash)
    );
    assert_eq!(decoded_response.payload_bytes, tree_state.payload_bytes);

    Ok(())
}

#[tokio::test]
async fn sparse_tree_state_rejects_upstream_time_that_disagrees_with_canonical_header()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let block_id = BlockId::new(block.height, block.block_hash);
    let canonical_block_time = u32::try_from(block.block_time)?;
    let replay = encode_fixture_block_replay(&block, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    ))?;
    let upstream = FixedTreeStateUpstream(SourceTreeState::new(
        block_id,
        canonical_block_time.saturating_add(1),
        br#"{"sapling":{"commitments":{"finalState":"aa"}}}"#.to_vec(),
    ));
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_tree_state_upstream(Arc::new(upstream));

    let error = match wallet_query.tree_state_at(block_id.height, None).await {
        Ok(tree_state) => {
            return Err(eyre!(
                "conflicting upstream time must fail closed, got {tree_state:?}"
            ));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::Node(SourceError::SourceProtocolMismatch {
            reason: "tree-state source time does not match the canonical block"
        })
    ));

    Ok(())
}

#[tokio::test]
async fn sparse_tree_state_uses_canonical_identity_and_time_with_frontier_only_payload()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let block_id = BlockId::new(block.height, block.block_hash);
    let canonical_block_time = u32::try_from(block.block_time)?;
    let replay = encode_fixture_block_replay(&block, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    ))?;
    let payload = br#"{"sapling":{"commitments":{"finalState":"aa"}}}"#.to_vec();
    let upstream = FixedTreeStateUpstream(SourceTreeState::new(
        block_id,
        canonical_block_time,
        payload.clone(),
    ));
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_tree_state_upstream(Arc::new(upstream));

    let tree_state = wallet_query.tree_state_at(block_id.height, None).await?;

    assert_eq!(tree_state.height, block_id.height);
    assert_eq!(tree_state.block_hash, block_id.hash);
    assert_eq!(tree_state.block_time_seconds, canonical_block_time);
    assert_eq!(tree_state.payload_bytes, payload);

    Ok(())
}

#[tokio::test]
async fn subtree_roots_response_returns_valid_empty_range() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    let compact_block = compact_block_with_tree_sizes(block.height, block.block_hash, 0, 0);
    let replay = encode_fixture_block_replay(&block, &[]);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = subtree_roots_response(
        &wallet_query,
        SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            NonZeroU32::new(8).ok_or_else(|| eyre!("invalid max entries"))?,
        ),
        None,
    )
    .await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::SubtreeRootsResponse::decode(encoded_response.as_slice())?;

    assert_eq!(decoded_response.start_index, 0);
    assert!(decoded_response.subtree_roots.is_empty());

    Ok(())
}

#[tokio::test]
async fn subtree_roots_response_reports_unavailable_when_completed_root_is_missing()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0, 0);
    store.commit_chain_epoch(chain_epoch_artifacts_with_sapling_outputs(
        chain_epoch,
        block,
        65_536,
    )?)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let error = match subtree_roots_response(
        &wallet_query,
        SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        ),
        None,
    )
    .await
    {
        Ok(response) => return Err(eyre!("expected unavailable roots, got {response:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::ArtifactUnavailable {
            family: ArtifactFamily::SubtreeRoot,
            key: ArtifactKey::SubtreeRootIndex {
                protocol: ShieldedProtocol::Sapling,
                index
            }
        } if index == SubtreeRootIndex::new(0)
    ));

    Ok(())
}

#[tokio::test]
async fn subtree_roots_response_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0, 0);
    let subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x71; 32]),
        block.height,
        block.block_hash,
    );
    store.commit_chain_epoch(
        chain_epoch_artifacts_with_sapling_outputs(chain_epoch, block, 65_536)?
            .with_subtree_roots(vec![subtree_root.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = subtree_roots_response(
        &wallet_query,
        SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        ),
        None,
    )
    .await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::SubtreeRootsResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| eyre!("missing response chain epoch"))?;
    let response_subtree_root = decoded_response
        .subtree_roots
        .first()
        .ok_or_else(|| eyre!("missing response subtree root"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(
        decoded_response.shielded_protocol,
        wallet::ShieldedProtocol::Sapling as i32
    );
    assert_eq!(
        decoded_response.start_index,
        subtree_root.subtree_index.value()
    );
    assert_eq!(
        response_subtree_root.root_hash,
        subtree_root.root_hash.as_bytes()
    );
    assert_eq!(
        response_subtree_root.completing_block_hash,
        encode_rpc_block_hash_hex(subtree_root.completing_block_hash)
    );
    assert_eq!(
        response_subtree_root.completing_block_height,
        subtree_root.completing_block_height.value()
    );

    Ok(())
}

#[tokio::test]
async fn chain_epoch_rejects_tip_metadata_that_disagrees_with_compact_block() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0, 0);
    let compact_block = CompactBlockArtifact::empty(
        BlockId::new(block.height, block.block_hash),
        block.parent_hash,
        u32::try_from(block.block_time).unwrap_or_default(),
        CompactChainMetadata {
            sapling_commitment_tree_size: 0,
            orchard_commitment_tree_size: 0,
            ironwood_commitment_tree_size: 0,
        },
    );
    let replay = encode_fixture_block_replay(&block, &[]);

    let error = match store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    )) {
        Ok(outcome) => return Err(eyre!("expected metadata rejection, got {outcome:?}")),
        Err(error) => error,
    };
    assert!(error.to_string().contains("visible-tip compact metadata"));

    Ok(())
}

#[tokio::test]
async fn compact_block_range_reports_unavailable_artifact_without_node_repair() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let replay = encode_fixture_block_replay(&block, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let error = match wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await
    {
        Ok(compact_block_range) => {
            return Err(eyre!(
                "expected unavailable artifact, got {compact_block_range:?}"
            ));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::ArtifactUnavailable {
            family: ArtifactFamily::CompactBlock,
            key: ArtifactKey::BlockHeight(height)
        } if height == BlockHeight::new(2)
    ));

    Ok(())
}

#[tokio::test]
async fn tree_state_checkpoint_response_reports_unavailable_artifact_without_node_repair()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let replay = encode_fixture_block_replay(&block, &[]);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let error = match tree_state_at_response(&wallet_query, BlockHeight::new(1), None).await {
        Ok(tree_state) => {
            return Err(eyre!("expected unavailable artifact, got {tree_state:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::ArtifactUnavailable {
            family: ArtifactFamily::TreeState,
            key: ArtifactKey::BlockHeight(height)
        } if height == BlockHeight::new(1)
    ));

    Ok(())
}

#[tokio::test]
async fn compact_block_range_rejects_inverted_height_range() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));

    let error = match wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(2), BlockHeight::new(1)),
            None,
        )
        .await
    {
        Ok(compact_block_range) => {
            return Err(eyre!("expected invalid range, got {compact_block_range:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, QueryError::InvalidBlockRange { .. }));

    Ok(())
}

#[tokio::test]
async fn compact_block_range_rejects_ranges_above_configured_limit() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let wallet_query = WalletQuery::with_options(
        store,
        (),
        Arc::new(sample_regtest_upgrade_activations()),
        WalletQueryOptions {
            max_compact_block_range: NonZeroU32::new(1)
                .ok_or_else(|| eyre!("invalid range limit"))?,
            ..WalletQueryOptions::default()
        },
    );

    let error = match wallet_query
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await
    {
        Ok(compact_block_range) => {
            return Err(eyre!(
                "expected range limit error, got {compact_block_range:?}"
            ));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::CompactBlockRangeTooLarge { .. }
    ));

    Ok(())
}

fn compact_block_range_chunk(
    chain_epoch: ChainEpoch,
    compact_block: &CompactBlockArtifact,
) -> wallet::CompactBlocksInRangeChunk {
    wallet::CompactBlocksInRangeChunk {
        chain_view: Some(wallet::ChainView {
            chain_epoch: Some(wallet::ChainEpoch {
                chain_epoch_id: chain_epoch.id.value(),
                network_name: encode_zinder_native_chain_name(chain_epoch.network).to_owned(),
                artifact_schema_version: u32::from(chain_epoch.artifact_schema_version.value()),
                created_at_millis: chain_epoch.created_at.value(),
                visible_tip: Some(wallet::BlockTip {
                    height: chain_epoch.visible_tip_height.value(),
                    hash: encode_rpc_block_hash_hex(chain_epoch.visible_tip_hash),
                }),
                settled_tip: Some(wallet::BlockTip {
                    height: chain_epoch.settled_tip_height.value(),
                    hash: encode_rpc_block_hash_hex(chain_epoch.settled_tip_hash),
                }),
                sapling_commitment_tree_size: chain_epoch.tip_metadata.sapling_commitment_tree_size,
                orchard_commitment_tree_size: chain_epoch.tip_metadata.orchard_commitment_tree_size,
                ironwood_commitment_tree_size: chain_epoch
                    .tip_metadata
                    .ironwood_commitment_tree_size,
            }),
            indexed_tip: None,
            upstream_tip: None,
            materialized_views: None,
        }),
        compact_block: Some(zinder_proto::wire::compact_block_message(compact_block)),
    }
}
