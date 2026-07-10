#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use std::sync::Arc;
use zinder_core::{
    ArtifactSchemaVersion, BlockHeight, ChainEpoch, ChainEpochId, ChainTipMetadata, Network,
    TransactionId, TxStatus, UnixTimestampMillis,
};
use zinder_query::{QueryError, WalletQuery, WalletQueryApi};
use zinder_store::{ChainEpochArtifacts, ReorgWindowChange};
use zinder_testkit::{FixtureTransactionRows, StoreFixture, sample_regtest_upgrade_activations};

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

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .compact_block_at(BlockHeight::new(1), None)
        .await?;

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

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let error = match wallet_query
        .compact_block_at(BlockHeight::new(99), None)
        .await
    {
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
        visible_tip_height: checkpoint_height,
        visible_tip_hash: checkpoint_hash,
        settled_tip_height: checkpoint_height,
        settled_tip_hash: checkpoint_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(13),
        tip_metadata: ChainTipMetadata::new(130_002, 39_758, 0),
        created_at: UnixTimestampMillis::new(1_774_668_000_000),
    };

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(
            checkpoint_epoch,
            Vec::<zinder_core::BlockHeaderArtifact>::new(),
            Vec::new(),
        )
        .with_reorg_window_change(ReorgWindowChange::AdvanceSafeTipTo {
            height: checkpoint_height,
        }),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let error = match wallet_query.compact_block_at(checkpoint_height, None).await {
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
    let transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        block.height,
        block.block_hash,
        0,
        b"raw-transaction-bytes".to_vec(),
    );
    let transaction_location = transaction_rows.location;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_block_transaction_index(vec![transaction_rows.block_transaction_index])
            .with_transaction_locations(vec![transaction_rows.location])
            .with_transaction_facts(vec![transaction_rows.facts])
            .with_transaction_blobs(transaction_rows.blob.into_iter().collect()),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query.transaction(transaction_id, None).await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    let TxStatus::Mined(mined) = response.status else {
        return Err(eyre!("expected mined transaction status, got {response:?}"));
    };
    assert_eq!(
        mined.location.transaction_id,
        transaction_location.transaction_id
    );
    assert_eq!(
        mined.location.block_height,
        transaction_location.block_height
    );
    assert_eq!(mined.location.block_hash, transaction_location.block_hash);
    assert_eq!(mined.location.tx_index_in_block, 0);
    assert_eq!(
        mined.raw_transaction_bytes,
        Some(b"raw-transaction-bytes".to_vec()),
        "the mined arm must carry the serialized transaction bytes alongside the location",
    );
    assert!(
        mined.details.confirmations >= 1,
        "a mined transaction at the visible tip has at least one confirmation",
    );

    Ok(())
}

#[tokio::test]
async fn transaction_omits_bytes_when_blob_is_not_retained() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let transaction_id = TransactionId::from_bytes([0xAB; 32]);
    let transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        block.height,
        block.block_hash,
        0,
        b"raw-transaction-bytes".to_vec(),
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_block_transaction_index(vec![transaction_rows.block_transaction_index])
            .with_transaction_locations(vec![transaction_rows.location])
            .with_transaction_facts(vec![transaction_rows.facts]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query.transaction(transaction_id, None).await?;

    let TxStatus::Mined(mined) = response.status else {
        return Err(eyre!("expected mined transaction status, got {response:?}"));
    };
    assert!(
        mined.raw_transaction_bytes.is_none(),
        "raw bytes are absent when the deployment does not retain transaction blobs",
    );

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

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transaction(TransactionId::from_bytes([0xCD; 32]), None)
        .await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert!(
        matches!(response.status, TxStatus::NotFound),
        "expected NotFound status, got {:?}",
        response.status
    );

    Ok(())
}
