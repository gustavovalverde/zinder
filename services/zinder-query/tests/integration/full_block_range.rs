#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use eyre::eyre;
use prost::Message;
use zinder_core::wire::encode_rpc_block_hash_hex;
use zinder_core::{BlockBlobArtifact, BlockHeight, BlockHeightRange};
use zinder_proto::v1::wallet;
use zinder_query::{ArtifactKey, QueryError, WalletQuery, WalletQueryApi};
use zinder_store::{ArtifactFamily, ChainEpochArtifacts};
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{block_hash_from_seed, synthetic_chain_epoch};

fn block_blob_for(height: u32) -> BlockBlobArtifact {
    BlockBlobArtifact::new(
        BlockHeight::new(height),
        block_hash_from_seed(height),
        block_hash_from_seed(height.saturating_sub(1)),
        format!("raw-block-{height}").into_bytes(),
    )
}

#[tokio::test]
async fn full_block_at_returns_serialized_bytes_height_and_hash() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let block_blob = block_blob_for(1);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_block_blobs(vec![block_blob.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let full_block = wallet_query
        .full_block_at(BlockHeight::new(1), None)
        .await?;

    assert_eq!(full_block.chain_epoch, chain_epoch);
    assert_eq!(full_block.block_blob, block_blob);

    Ok(())
}

#[tokio::test]
async fn full_block_range_streams_blocks_in_order() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);
    let first_blob = block_blob_for(1);
    let second_blob = block_blob_for(2);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(first_epoch, vec![first_block], vec![first_compact_block])
            .with_block_blobs(vec![first_blob.clone()]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(second_epoch, vec![second_block], vec![second_compact_block])
            .with_block_blobs(vec![second_blob.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let full_block_range = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await?;

    assert_eq!(full_block_range.chain_epoch, second_epoch);
    assert_eq!(full_block_range.block_blobs, vec![first_blob, second_blob]);

    Ok(())
}

#[tokio::test]
async fn full_block_at_unretained_height_returns_artifact_unavailable() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let outcome = wallet_query.full_block_at(BlockHeight::new(1), None).await;

    assert!(
        matches!(
            outcome,
            Err(QueryError::ArtifactUnavailable {
                family: ArtifactFamily::BlockBlob,
                key: ArtifactKey::BlockHeight(height),
            }) if height == BlockHeight::new(1)
        ),
        "unretained block blob must degrade to ArtifactUnavailable, got {outcome:?}"
    );

    Ok(())
}

#[tokio::test]
async fn full_block_range_unretained_height_returns_artifact_unavailable() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (first_epoch, first_block, first_compact_block) = synthetic_chain_epoch(1, 1);
    let (second_epoch, second_block, second_compact_block) = synthetic_chain_epoch(2, 2);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(first_epoch, vec![first_block], vec![first_compact_block])
            .with_block_blobs(vec![block_blob_for(1)]),
    )?;
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        second_epoch,
        vec![second_block],
        vec![second_compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let outcome = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await;

    assert!(
        matches!(
            outcome,
            Err(QueryError::ArtifactUnavailable {
                family: ArtifactFamily::BlockBlob,
                key: ArtifactKey::BlockHeight(height),
            }) if height == BlockHeight::new(2)
        ),
        "first unretained height in the range must abort with ArtifactUnavailable, got {outcome:?}"
    );

    Ok(())
}

#[tokio::test]
async fn full_block_range_chunk_uses_native_wallet_proto_shape() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let block_blob = block_blob_for(1);

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_block_blobs(vec![block_blob.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let full_block_range = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(1)),
            None,
        )
        .await?;
    let response_block_blob = full_block_range
        .block_blobs
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing full block"))?;
    let chunk = wallet::FullBlocksInRangeChunk {
        chain_view: Some(zinder_store::chain_view_message(
            full_block_range.chain_epoch,
        )),
        full_block: Some(wallet::FullBlock {
            height: response_block_blob.height.value(),
            block_hash: encode_rpc_block_hash_hex(response_block_blob.block_hash),
            payload_bytes: response_block_blob.raw_block_bytes.clone(),
            parent_block_hash: encode_rpc_block_hash_hex(response_block_blob.parent_hash),
        }),
    };
    let decoded_chunk = wallet::FullBlocksInRangeChunk::decode(chunk.encode_to_vec().as_slice())?;
    let response_chain_epoch = decoded_chunk
        .chain_view
        .as_ref()
        .and_then(|chain_view| chain_view.chain_epoch.as_ref())
        .ok_or_else(|| eyre!("missing chunk chain epoch"))?;
    let response_full_block = decoded_chunk
        .full_block
        .as_ref()
        .ok_or_else(|| eyre!("missing chunk full block"))?;

    assert_eq!(response_chain_epoch.chain_epoch_id, chain_epoch.id.value());
    assert_eq!(response_full_block.height, block_blob.height.value());
    assert_eq!(
        response_full_block.block_hash,
        encode_rpc_block_hash_hex(block_blob.block_hash)
    );
    assert_eq!(
        response_full_block.parent_block_hash,
        encode_rpc_block_hash_hex(block_blob.parent_hash)
    );
    assert_eq!(
        response_full_block.payload_bytes,
        block_blob.raw_block_bytes
    );

    Ok(())
}
