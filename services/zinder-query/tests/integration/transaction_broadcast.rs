#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use std::sync::Arc;
use zinder_core::{
    BroadcastAccepted, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastQueued,
    BroadcastRejected, BroadcastRejectionReason, MAX_RAW_TRANSACTION_BYTES, Network,
    RawTransactionBytes, TransactionBroadcastResult, TransactionId,
};
use zinder_query::{QueryError, WalletQuery, WalletQueryApi};
use zinder_source::SourceError;
use zinder_testkit::{
    MockTransactionBroadcaster, StoreFixture, sample_regtest_upgrade_activations,
};

#[tokio::test]
async fn broadcast_transaction_returns_accepted_result() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([7; 32]);
    let mock_broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        mock_broadcaster.clone(),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let broadcast_result = wallet_query
        .broadcast_transaction(RawTransactionBytes::new([0x00, 0x01]))
        .await?;

    assert_eq!(
        broadcast_result,
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id })
    );
    assert_eq!(
        mock_broadcaster.captured_calls(),
        vec![RawTransactionBytes::new([0x00, 0x01])]
    );

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_preserves_rejection_classes() -> eyre::Result<()> {
    let cases = [
        TransactionBroadcastResult::InvalidEncoding(BroadcastInvalidEncoding {
            error_code: Some(-22),
            message: "TX decode failed".to_owned(),
        }),
        TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
            error_code: Some(-27),
            message: "transaction already in mempool".to_owned(),
        }),
        TransactionBroadcastResult::Queued(BroadcastQueued {
            message: "already queued for download".to_owned(),
        }),
        TransactionBroadcastResult::Rejected(BroadcastRejected {
            kind: BroadcastRejectionReason::InvalidSignature,
            error_code: Some(-25),
            message: "transaction signature is invalid".to_owned(),
        }),
        TransactionBroadcastResult::Rejected(BroadcastRejected {
            kind: BroadcastRejectionReason::Unknown,
            error_code: Some(-25),
            message: "bad-txns-invalid".to_owned(),
        }),
    ];

    for expected_broadcast_result in cases {
        let store_fixture = StoreFixture::open()?;
        let wallet_query = WalletQuery::new(
            store_fixture.chain_store().clone(),
            MockTransactionBroadcaster::returning(expected_broadcast_result.clone()),
            Arc::new(sample_regtest_upgrade_activations()),
        );

        let broadcast_result = wallet_query
            .broadcast_transaction(RawTransactionBytes::new([0x02]))
            .await?;

        assert_eq!(broadcast_result, expected_broadcast_result);
    }

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_rejects_oversized_payload_before_upstream_call() -> eyre::Result<()>
{
    let mock_broadcaster = MockTransactionBroadcaster::accepted(TransactionId::from_bytes([1; 32]));
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        mock_broadcaster.clone(),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let oversized = RawTransactionBytes::new(vec![0u8; MAX_RAW_TRANSACTION_BYTES + 1]);

    let error = match wallet_query.broadcast_transaction(oversized).await {
        Ok(broadcast_result) => {
            return Err(eyre!("expected size error, got {broadcast_result:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::BroadcastTransactionTooLarge { actual, maximum }
            if actual == MAX_RAW_TRANSACTION_BYTES + 1 && maximum == MAX_RAW_TRANSACTION_BYTES
    ));
    assert!(
        mock_broadcaster.captured_calls().is_empty(),
        "oversized payload must be rejected before reaching the broadcaster"
    );

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_admits_payload_at_size_bound() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([6; 32]);
    let mock_broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        mock_broadcaster.clone(),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let at_bound = RawTransactionBytes::new(vec![0u8; MAX_RAW_TRANSACTION_BYTES]);

    let broadcast_result = wallet_query.broadcast_transaction(at_bound.clone()).await?;

    assert_eq!(
        broadcast_result,
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id })
    );
    assert_eq!(mock_broadcaster.captured_calls(), vec![at_bound]);

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_maps_node_unavailability_to_query_error() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        MockTransactionBroadcaster::node_unavailable("node is offline"),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let error = match wallet_query
        .broadcast_transaction(RawTransactionBytes::new([0x03]))
        .await
    {
        Ok(broadcast_result) => {
            return Err(eyre!("expected node error, got {broadcast_result:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(
        error,
        QueryError::Node(SourceError::NodeUnavailable { ref reason })
            if reason == "node is offline"
    ));

    Ok(())
}

#[tokio::test]
async fn broadcast_transaction_does_not_mutate_chain_epoch() -> eyre::Result<()> {
    let store_fixture = StoreFixture::with_single_block(Network::ZcashRegtest)?;
    let chain_epoch = *store_fixture
        .committed_chain_epoch()
        .ok_or_else(|| eyre!("StoreFixture::with_single_block must commit a chain epoch"))?;
    let store = store_fixture.chain_store().clone();

    let transaction_id = TransactionId::from_bytes([9; 32]);
    let wallet_query = WalletQuery::new(
        store.clone(),
        MockTransactionBroadcaster::accepted(transaction_id),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let broadcast_result = wallet_query
        .broadcast_transaction(RawTransactionBytes::new([0x04]))
        .await?;
    let reader = store.current_chain_epoch_reader()?;

    assert_eq!(
        broadcast_result,
        TransactionBroadcastResult::Accepted(BroadcastAccepted { transaction_id })
    );
    assert_eq!(store.current_chain_epoch()?, Some(chain_epoch));
    assert!(reader.transaction_facts_by_id(transaction_id)?.is_none());

    Ok(())
}

#[tokio::test]
async fn read_only_wallet_query_reports_broadcast_disabled() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );

    let error = match wallet_query
        .broadcast_transaction(RawTransactionBytes::new([0x05]))
        .await
    {
        Ok(broadcast_result) => {
            return Err(eyre!("expected disabled error, got {broadcast_result:?}"));
        }
        Err(error) => error,
    };

    assert!(matches!(error, QueryError::TransactionBroadcastDisabled));

    Ok(())
}
