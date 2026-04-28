#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::num::NonZeroU32;

use eyre::eyre;
use prost::Message;
use zinder_core::{
    BlockHeight, BlockHeightRange, ChainEpoch, ChainTipMetadata, CompactBlockArtifact,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, SubtreeRootRange,
    TreeStateArtifact,
};
use zinder_proto::v1::wallet;
use zinder_query::{
    ArtifactKey, QueryError, WalletQuery, WalletQueryApi, WalletQueryOptions,
    latest_block_response, latest_tree_state_response, subtree_roots_response, tree_state_response,
};
use zinder_store::{ArtifactFamily, ChainEpochArtifacts};
use zinder_testkit::StoreFixture;

use crate::common::{compact_block_with_tree_sizes, synthetic_chain_epoch};

#[tokio::test]
async fn compact_block_range_reads_from_one_chain_epoch() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        first_epoch,
        vec![first_block],
        vec![first_compact_block.clone()],
    ))?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_compact_block.clone()],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let compact_block_range = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))
        .await?;

    assert_eq!(compact_block_range.chain_epoch, second_epoch);
    assert_eq!(
        compact_block_range.compact_blocks,
        vec![first_compact_block, second_compact_block]
    );

    Ok(())
}

#[tokio::test]
async fn compact_block_range_chunk_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block.clone()],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let compact_block_range = wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(1),
        ))
        .await?;
    let response_compact_block = compact_block_range
        .compact_blocks
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing compact block"))?;
    let chunk = compact_block_range_chunk(compact_block_range.chain_epoch, response_compact_block);
    let encoded_chunk = chunk.encode_to_vec();
    let decoded_chunk = wallet::CompactBlockRangeChunk::decode(encoded_chunk.as_slice())?;
    let response_chain_epoch = decoded_chunk
        .chain_epoch
        .as_ref()
        .ok_or_else(|| eyre!("missing chunk chain epoch"))?;
    let response_compact_block = decoded_chunk
        .compact_block
        .as_ref()
        .ok_or_else(|| eyre!("missing chunk compact block"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(
        response_chain_epoch.network_name,
        chain_epoch.network.name()
    );
    assert_eq!(
        response_chain_epoch.tip_height,
        chain_epoch.tip_height.value()
    );
    assert_eq!(
        response_chain_epoch.tip_hash,
        chain_epoch.tip_hash.as_bytes()
    );
    assert_eq!(
        response_chain_epoch.finalized_height,
        chain_epoch.finalized_height.value()
    );
    assert_eq!(
        response_chain_epoch.finalized_hash,
        chain_epoch.finalized_hash.as_bytes()
    );
    assert_eq!(
        response_chain_epoch.artifact_schema_version,
        u32::from(chain_epoch.artifact_schema_version.value())
    );
    assert_eq!(
        response_chain_epoch.created_at_millis,
        chain_epoch.created_at.value()
    );
    assert_eq!(response_compact_block.height, compact_block.height.value());
    assert_eq!(
        response_compact_block.block_hash,
        compact_block.block_hash.as_bytes()
    );
    assert_eq!(
        response_compact_block.payload_bytes,
        compact_block.payload_bytes
    );

    Ok(())
}

#[tokio::test]
async fn latest_block_response_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let response = latest_block_response(&wallet_query, None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::LatestBlockResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_epoch
        .as_ref()
        .ok_or_else(|| eyre!("missing response chain epoch"))?;
    let latest_block = decoded_response
        .latest_block
        .as_ref()
        .ok_or_else(|| eyre!("missing latest block metadata"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(latest_block.height, chain_epoch.tip_height.value());
    assert_eq!(latest_block.block_hash, chain_epoch.tip_hash.as_bytes());

    Ok(())
}

#[tokio::test]
async fn tree_state_response_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        BlockHeight::new(1),
        chain_epoch.tip_hash,
        b"tree-state-1".to_vec(),
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_tree_states(vec![tree_state.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let response = tree_state_response(&wallet_query, BlockHeight::new(1), None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::TreeStateResponse::decode(encoded_response.as_slice())?;
    let response_chain_epoch = decoded_response
        .chain_epoch
        .as_ref()
        .ok_or_else(|| eyre!("missing response chain epoch"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(decoded_response.height, tree_state.height.value());
    assert_eq!(
        decoded_response.block_hash,
        tree_state.block_hash.as_bytes()
    );
    assert_eq!(decoded_response.payload_bytes, tree_state.payload_bytes);

    Ok(())
}

#[tokio::test]
async fn latest_tree_state_response_uses_tip_tree_state() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let tree_state = TreeStateArtifact::new(
        BlockHeight::new(1),
        chain_epoch.tip_hash,
        b"tree-state-1".to_vec(),
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_tree_states(vec![tree_state.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let response = latest_tree_state_response(&wallet_query, None).await?;
    let encoded_response = response.encode_to_vec();
    let decoded_response = wallet::TreeStateResponse::decode(encoded_response.as_slice())?;

    assert_eq!(decoded_response.height, tree_state.height.value());
    assert_eq!(
        decoded_response.block_hash,
        tree_state.block_hash.as_bytes()
    );
    assert_eq!(decoded_response.payload_bytes, tree_state.payload_bytes);

    Ok(())
}

#[tokio::test]
async fn subtree_roots_response_returns_valid_empty_range() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    let compact_block = compact_block_with_tree_sizes(block.height, block.block_hash, 0, 0);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
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
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0);
    let compact_block = compact_block_with_tree_sizes(block.height, block.block_hash, 65_536, 0);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
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
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0);
    let compact_block = compact_block_with_tree_sizes(block.height, block.block_hash, 65_536, 0);
    let subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x71; 32]),
        block.height,
        block.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_subtree_roots(vec![subtree_root.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
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
        .chain_epoch
        .as_ref()
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
        subtree_root.completing_block_hash.as_bytes()
    );
    assert_eq!(
        response_subtree_root.completing_block_height,
        subtree_root.completing_block_height.value()
    );

    Ok(())
}

#[tokio::test]
async fn subtree_roots_response_uses_stored_tip_metadata_not_compact_block_payload()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (mut chain_epoch, block, _compact_block) = synthetic_chain_epoch(1, 1);
    chain_epoch.tip_metadata = ChainTipMetadata::new(65_536, 0);
    let compact_block =
        CompactBlockArtifact::new(block.height, block.block_hash, b"not-protobuf".to_vec());
    let subtree_root = SubtreeRootArtifact::new(
        ShieldedProtocol::Sapling,
        SubtreeRootIndex::new(0),
        SubtreeRootHash::from_bytes([0x71; 32]),
        block.height,
        block.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_subtree_roots(vec![subtree_root.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let response = wallet_query
        .subtree_roots(SubtreeRootRange::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            NonZeroU32::new(1).ok_or_else(|| eyre!("invalid max entries"))?,
        ))
        .await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert_eq!(response.subtree_roots, vec![subtree_root]);

    Ok(())
}

#[tokio::test]
async fn compact_block_range_reports_unavailable_artifact_without_node_repair() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let error = match wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))
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
async fn tree_state_response_reports_unavailable_artifact_without_node_repair() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let error = match tree_state_response(&wallet_query, BlockHeight::new(1), None).await {
        Ok(tree_state_response) => {
            return Err(eyre!(
                "expected unavailable artifact, got {tree_state_response:?}"
            ));
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
    let wallet_query = WalletQuery::new(store, ());

    let error = match wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(2),
            BlockHeight::new(1),
        ))
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
        WalletQueryOptions {
            max_compact_block_range: NonZeroU32::new(1)
                .ok_or_else(|| eyre!("invalid range limit"))?,
        },
    );

    let error = match wallet_query
        .compact_block_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))
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
    compact_block: CompactBlockArtifact,
) -> wallet::CompactBlockRangeChunk {
    wallet::CompactBlockRangeChunk {
        chain_epoch: Some(wallet::ChainEpoch {
            chain_epoch_id: chain_epoch.id.value(),
            network_name: chain_epoch.network.name().to_owned(),
            tip_height: chain_epoch.tip_height.value(),
            tip_hash: chain_epoch.tip_hash.as_bytes().into(),
            finalized_height: chain_epoch.finalized_height.value(),
            finalized_hash: chain_epoch.finalized_hash.as_bytes().into(),
            artifact_schema_version: u32::from(chain_epoch.artifact_schema_version.value()),
            created_at_millis: chain_epoch.created_at.value(),
            sapling_commitment_tree_size: chain_epoch.tip_metadata.sapling_commitment_tree_size,
            orchard_commitment_tree_size: chain_epoch.tip_metadata.orchard_commitment_tree_size,
        }),
        compact_block: Some(wallet::CompactBlock {
            height: compact_block.height.value(),
            block_hash: compact_block.block_hash.as_bytes().into(),
            payload_bytes: compact_block.payload_bytes,
        }),
    }
}
