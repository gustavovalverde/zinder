#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use arc_swap::ArcSwap;
use zinder_core::{BlockHeight, BlockId, BlockSelector, ChainEpochId, Network};
use zinder_query::{QueryError, WalletQueryApi, WalletServingQuery, WalletServingReadPair};
use zinder_testkit::{
    ChainFixture, MockTransactionBroadcaster, WalletServingStoreFixture,
    sample_regtest_upgrade_activations,
};

#[tokio::test]
async fn block_id_by_selector_resolves_a_non_tip_canonical_hash() -> eyre::Result<()> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let expected_block = chain
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("selector fixture must include block 1"))?;
    let (_store_fixture, query) = build_query(&chain)?;

    let response = query
        .block_id_by_selector(BlockSelector::from_hash(expected_block.hash), None)
        .await?;

    assert_eq!(
        response.block_id,
        BlockId::new(expected_block.height, expected_block.hash)
    );
    assert_eq!(response.chain_epoch.visible_tip_height, BlockHeight::new(3));
    Ok(())
}

#[tokio::test]
async fn block_header_by_selector_resolves_a_non_tip_canonical_hash() -> eyre::Result<()> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let expected_block = chain
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre::eyre!("selector fixture must include block 2"))?;
    let (_store_fixture, query) = build_query(&chain)?;

    let response = query
        .block_header_by_selector(BlockSelector::from_hash(expected_block.hash), None)
        .await?;

    assert_eq!(
        response.block_header.block_id,
        BlockId::new(expected_block.height, expected_block.hash)
    );
    assert_eq!(response.chain_epoch.visible_tip_height, BlockHeight::new(3));
    Ok(())
}

#[tokio::test]
async fn hash_selectors_reject_a_mismatched_pinned_epoch() -> eyre::Result<()> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(3);
    let selected_block = chain
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre::eyre!("selector fixture must include block 1"))?;
    let (_store_fixture, query) = build_query(&chain)?;
    let visible = query.visible_tip_block(None).await?;
    let mismatched_epoch = ChainEpochId::new(visible.chain_epoch.id.value().saturating_add(1));

    let block_id_outcome = query
        .block_id_by_selector(
            BlockSelector::from_hash(selected_block.hash),
            Some(mismatched_epoch),
        )
        .await;
    let block_id_error = match block_id_outcome {
        Err(error) => error,
        Ok(response) => {
            return Err(eyre::eyre!(
                "block-id lookup unexpectedly accepted a mismatched epoch pin: {response:?}"
            ));
        }
    };
    let block_header_outcome = query
        .block_header_by_selector(
            BlockSelector::from_hash(selected_block.hash),
            Some(mismatched_epoch),
        )
        .await;
    let block_header_error = match block_header_outcome {
        Err(error) => error,
        Ok(response) => {
            return Err(eyre::eyre!(
                "block-header lookup unexpectedly accepted a mismatched epoch pin: {response:?}"
            ));
        }
    };

    assert!(matches!(
        block_id_error,
        QueryError::ChainEpochPinUnavailable { chain_epoch_id }
            if chain_epoch_id == mismatched_epoch
    ));
    assert!(matches!(
        block_header_error,
        QueryError::ChainEpochPinUnavailable { chain_epoch_id }
            if chain_epoch_id == mismatched_epoch
    ));
    Ok(())
}

fn build_query(
    chain: &ChainFixture,
) -> eyre::Result<(
    WalletServingStoreFixture,
    WalletServingQuery<MockTransactionBroadcaster>,
)> {
    let activations = Arc::new(sample_regtest_upgrade_activations());
    let mut store_fixture = WalletServingStoreFixture::from_chain(chain, activations.as_ref())?;
    let (canonical_reader, wallet_reader) = store_fixture.take_readers()?;
    let serving_pair = Arc::new(WalletServingReadPair::new(
        Arc::new(canonical_reader),
        Arc::new(wallet_reader),
    )?);
    let serving_pair_slot = Arc::new(ArcSwap::from(serving_pair));
    let query = WalletServingQuery::from_serving_pair_slot(
        serving_pair_slot,
        MockTransactionBroadcaster::broadcast_disabled(),
        activations,
    );
    Ok((store_fixture, query))
}
