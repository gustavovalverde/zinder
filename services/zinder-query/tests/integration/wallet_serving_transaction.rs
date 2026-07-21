#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use arc_swap::ArcSwap;
use tonic::Request;
use zinder_core::{Network, TransactionId, TxStatus};
use zinder_proto::capabilities::WALLET_READ_TRANSACTION_BYTES_V1;
use zinder_proto::v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService};
use zinder_query::{
    ServerInfoSettings, WalletCapabilityProfile, WalletQueryApi, WalletQueryGrpcAdapter,
    WalletServingQuery, WalletServingReadPair,
};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, MockTransactionBroadcaster, WalletServingStoreFixture,
    sample_regtest_upgrade_activations,
};

#[tokio::test]
async fn wallet_serving_transaction_returns_retained_bytes_from_ready_secondaries()
-> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x51; 32]);
    let raw_transaction_bytes = vec![0x05, 0x00, 0x00, 0x80, 0x0a, 0x0b];
    let chain = chain_with_transaction(transaction_id, raw_transaction_bytes.clone())?;
    let (_store_fixture, query) = build_query(&chain)?;

    let response = query.transaction(transaction_id, None).await?;
    let TxStatus::Mined(mined) = response.status else {
        return Err(eyre::eyre!("retained transaction must be mined"));
    };

    assert_eq!(mined.raw_transaction_bytes, Some(raw_transaction_bytes));
    Ok(())
}

#[tokio::test]
async fn exact_pair_server_info_advertises_retained_transaction_bytes() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x52; 32]);
    let chain = chain_with_transaction(transaction_id, vec![0x01, 0x02, 0x03])?;
    let (_store_fixture, query) = build_query(&chain)?;
    let adapter = WalletQueryGrpcAdapter::new(
        query,
        ServerInfoSettings {
            transaction_blobs_retained: true,
            capability_profile: WalletCapabilityProfile::ExactPair,
            ..ServerInfoSettings::default()
        },
    );

    let info =
        WalletQueryService::server_info(&adapter, Request::new(wallet::ServerInfoRequest {}))
            .await?
            .into_inner()
            .info
            .ok_or_else(|| eyre::eyre!("missing exact-pair server info"))?;
    let capabilities = info
        .common
        .ok_or_else(|| eyre::eyre!("missing common server info"))?
        .capabilities;

    assert!(
        capabilities
            .iter()
            .any(|capability| capability == WALLET_READ_TRANSACTION_BYTES_V1)
    );
    Ok(())
}

#[test]
fn wallet_serving_ready_fixture_rejects_missing_transaction_bytes() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x53; 32]);
    let mut chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (tip_height, tip_hash) = chain
        .blocks()
        .last()
        .map(|tip| (tip.height, tip.hash))
        .ok_or_else(|| eyre::eyre!("fixture must contain one block"))?;
    let mut transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        tip_height,
        tip_hash,
        0,
        [0x01, 0x02],
    );
    transaction_rows.blob = None;
    chain = chain.with_transaction_rows(transaction_rows);

    let outcome =
        WalletServingStoreFixture::from_chain(&chain, &sample_regtest_upgrade_activations());
    let error = outcome
        .err()
        .ok_or_else(|| eyre::eyre!("wallet-serving READY fixture accepted a missing blob"))?;

    assert!(error.to_string().contains("has no raw blob"));
    Ok(())
}

fn chain_with_transaction(
    transaction_id: TransactionId,
    raw_transaction_bytes: Vec<u8>,
) -> eyre::Result<ChainFixture> {
    let chain = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (tip_height, tip_hash) = chain
        .blocks()
        .last()
        .map(|tip| (tip.height, tip.hash))
        .ok_or_else(|| eyre::eyre!("fixture must contain one block"))?;
    Ok(
        chain.with_transaction_rows(FixtureTransactionRows::from_raw_transaction(
            transaction_id,
            tip_height,
            tip_hash,
            0,
            raw_transaction_bytes,
        )),
    )
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
