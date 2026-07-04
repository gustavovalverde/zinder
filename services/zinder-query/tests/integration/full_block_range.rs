#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use eyre::eyre;
use prost::Message;
use zinder_core::wire::encode_rpc_block_hash_hex;
use zinder_core::{
    BlockBlobArtifact, BlockHeight, BlockHeightRange, ChainEpoch, ChainEpochId, Network,
};
use zinder_proto::v1::wallet;
use zinder_query::{
    ArtifactKey, DEFAULT_MAX_FULL_BLOCK_RANGE, FullBlockStream, QueryError, WalletQuery,
    WalletQueryApi,
};
use zinder_store::{ArtifactFamily, ChainEpochArtifacts};
use zinder_testkit::{ChainFixture, StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{block_hash_from_seed, synthetic_chain_epoch};

/// Drains a full-block stream into its pinned epoch, the blobs delivered in
/// order, and the single terminal error if the stream ended with one.
async fn drain_full_block_stream(
    mut stream: FullBlockStream,
) -> (ChainEpoch, Vec<BlockBlobArtifact>, Option<QueryError>) {
    let chain_epoch = stream.chain_epoch;
    let mut blobs = Vec::new();
    let mut terminal_error = None;
    while let Some(block) = stream.blocks.recv().await {
        match block {
            Ok(blob) => blobs.push(blob),
            Err(error) => {
                terminal_error = Some(error);
                break;
            }
        }
    }
    (chain_epoch, blobs, terminal_error)
}

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
    let stream = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await?;
    let (chain_epoch, blobs, terminal_error) = drain_full_block_stream(stream).await;

    assert_eq!(chain_epoch, second_epoch);
    assert_eq!(blobs, vec![first_blob, second_blob]);
    assert!(terminal_error.is_none(), "clean range must not error");

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
async fn full_block_range_missing_blob_delivers_prefix_then_error() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();

    for height in 1..=5u32 {
        let (chain_epoch, block, compact_block) = synthetic_chain_epoch(u64::from(height), height);
        let artifacts = ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block]);
        // Height 4 is committed without a blob; every other height retains one.
        let artifacts = if height == 4 {
            artifacts
        } else {
            artifacts.with_block_blobs(vec![block_blob_for(height)])
        };
        store.commit_chain_epoch(artifacts)?;
    }

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let stream = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(5)),
            None,
        )
        .await?;
    let (_chain_epoch, blobs, terminal_error) = drain_full_block_stream(stream).await;

    assert_eq!(
        blobs,
        vec![block_blob_for(1), block_blob_for(2), block_blob_for(3)],
        "every chunk before the gap must be delivered intact"
    );
    assert!(
        matches!(
            terminal_error,
            Some(QueryError::ArtifactUnavailable {
                family: ArtifactFamily::BlockBlob,
                key: ArtifactKey::BlockHeight(height),
            }) if height == BlockHeight::new(4)
        ),
        "the missing mid-range height must terminate the stream, got {terminal_error:?}"
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
    let stream = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(1)),
            None,
        )
        .await?;
    let (stream_chain_epoch, blobs, terminal_error) = drain_full_block_stream(stream).await;
    assert!(
        terminal_error.is_none(),
        "single retained block must not error"
    );
    let response_block_blob = blobs
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("missing full block"))?;
    let chunk = wallet::FullBlocksInRangeChunk {
        chain_view: Some(zinder_store::chain_view_message(stream_chain_epoch)),
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

#[tokio::test]
async fn full_block_range_streams_one_thousand_blocks_under_one_epoch() -> eyre::Result<()> {
    let block_count = DEFAULT_MAX_FULL_BLOCK_RANGE.get();
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(block_count);
    let store_fixture = StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let stream = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(block_count)),
            None,
        )
        .await?;
    let (chain_epoch, blobs, terminal_error) = drain_full_block_stream(stream).await;

    assert!(terminal_error.is_none(), "retained range must not error");
    assert_eq!(chain_epoch.id, ChainEpochId::new(1));
    assert_eq!(blobs.len(), usize::try_from(block_count)?);
    for (offset, blob) in blobs.iter().enumerate() {
        let expected = BlockHeight::new(u32::try_from(offset)?.saturating_add(1));
        assert_eq!(
            blob.height, expected,
            "blocks must be ascending and contiguous"
        );
    }

    Ok(())
}

#[tokio::test]
async fn full_block_range_above_cap_is_rejected() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_block_blobs(vec![block_blob_for(1)]),
    )?;

    let over_cap = DEFAULT_MAX_FULL_BLOCK_RANGE.get().saturating_add(1);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let outcome = wallet_query
        .full_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(over_cap)),
            None,
        )
        .await;

    assert!(
        matches!(
            outcome,
            Err(QueryError::CompactBlockRangeTooLarge { requested, maximum })
                if requested == usize::try_from(over_cap)?
                    && maximum == usize::try_from(DEFAULT_MAX_FULL_BLOCK_RANGE.get())?
        ),
        "an over-cap range must be rejected before streaming, got {outcome:?}"
    );

    Ok(())
}
