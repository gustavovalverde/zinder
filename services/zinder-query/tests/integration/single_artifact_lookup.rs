#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use zinder_core::{
    ArtifactSchemaVersion, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, Network,
    TransactionArtifact, TransactionId, UnixTimestampMillis,
};
use zinder_query::{ArtifactKey, QueryError, WalletQuery, WalletQueryApi};
use zinder_store::{ArtifactFamily, ChainEpochArtifacts, ReorgWindowChange};
use zinder_testkit::StoreFixture;

use crate::common::{block_hash_from_seed, synthetic_chain_epoch};

#[tokio::test]
async fn compact_block_at_returns_indexed_block() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block.clone()],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let response = wallet_query.compact_block_at(BlockHeight::new(1)).await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert_eq!(response.compact_block, compact_block);

    Ok(())
}

#[tokio::test]
async fn compact_block_at_reports_unavailable_for_missing_height() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let error = match wallet_query.compact_block_at(BlockHeight::new(99)).await {
        Ok(response) => return Err(eyre!("expected unavailable, got {response:?}")),
        Err(error) => error,
    };

    assert!(matches!(error, QueryError::ArtifactUnavailable { .. }));

    Ok(())
}

#[tokio::test]
async fn compact_block_at_reports_unavailable_below_checkpoint() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let checkpoint_height = BlockHeight::new(1_000);
    let checkpoint_hash = block_hash_from_seed(1_000);
    let checkpoint_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        tip_height: checkpoint_height,
        tip_hash: checkpoint_hash,
        finalized_height: checkpoint_height,
        finalized_hash: checkpoint_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(1),
        tip_metadata: ChainTipMetadata::new(130_002, 39_758),
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(checkpoint_epoch, Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::FinalizeThrough {
                height: checkpoint_height,
            }),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let error = match wallet_query.compact_block_at(checkpoint_height).await {
        Ok(response) => return Err(eyre!("expected unavailable, got {response:?}")),
        Err(error) => error,
    };

    assert!(matches!(error, QueryError::ArtifactUnavailable { .. }));

    Ok(())
}

#[tokio::test]
async fn transaction_returns_indexed_transaction() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let transaction_id = TransactionId::from_bytes([0xAB; 32]);
    let transaction = TransactionArtifact::new(
        transaction_id,
        block.height,
        block.block_hash,
        b"raw-transaction-bytes".to_vec(),
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transactions(vec![transaction.clone()]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let response = wallet_query.transaction(transaction_id).await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert_eq!(response.transaction, transaction);

    Ok(())
}

#[tokio::test]
async fn transaction_reports_unavailable_for_unknown_id() -> eyre::Result<()> {
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
        .transaction(TransactionId::from_bytes([0xCD; 32]))
        .await
    {
        Ok(response) => return Err(eyre!("expected unavailable, got {response:?}")),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::ArtifactUnavailable {
            family: ArtifactFamily::Transaction,
            key: ArtifactKey::TransactionId(_)
        }
    ));

    Ok(())
}
