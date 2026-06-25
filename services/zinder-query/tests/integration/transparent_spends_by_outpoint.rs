#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use eyre::eyre;
use tonic::{Code, Request};
use zinder_core::{
    ChainEpochId, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentOutputArtifact, TransparentSpendFact,
};
use zinder_proto::v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter};
use zinder_store::ChainEpochArtifacts;
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::synthetic_chain_epoch;

/// Commits a spendable output then a spend of it.
///
/// The output lands in the first epoch and the spend in the second. Returns the
/// spent outpoint and the resolved spend fact so a test can assert the query
/// projects the same spending identity.
fn commit_spent_outpoint_fixture(
    store: &zinder_store::PrimaryChainStore,
) -> eyre::Result<(TransparentOutPoint, TransparentSpendFact)> {
    let (epoch_one, block_one, compact_one) = synthetic_chain_epoch(1, 1);
    let (epoch_two, block_two, compact_two) = synthetic_chain_epoch(2, 2);

    let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x31; 32]), 0);
    let script_pub_key = vec![0x76, 0xa9, 0x14, 0x88, 0xac];
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    let output = TransparentOutputArtifact::new(
        spent_outpoint,
        12_345_678,
        script_pub_key,
        address_script_hash,
        block_one.height,
        block_one.block_hash,
    );
    let spend = TransparentSpendFact::new(
        spent_outpoint,
        2,
        TransactionId::from_bytes([0x33; 32]),
        0,
        block_two.height,
        block_two.block_hash,
        output.value_zat,
        output.address_script_hash,
        output.block_height,
        output.block_hash,
    );

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_one, vec![block_one], vec![compact_one])
            .with_transparent_outputs_by_outpoint(vec![output]),
    )?;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_two, vec![block_two], vec![compact_two])
            .with_transparent_spend_facts(vec![spend.clone()]),
    )?;

    Ok((spent_outpoint, spend))
}

#[tokio::test]
async fn transparent_spends_by_outpoint_resolves_a_confirmed_spend() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (spent_outpoint, spend) = commit_spent_outpoint_fixture(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![spent_outpoint], None::<ChainEpochId>)
        .await?;

    assert_eq!(response.spends.len(), 1);
    let resolved = response
        .spends
        .first()
        .ok_or_else(|| eyre!("expected one resolved spend"))?;
    assert_eq!(resolved.spent_outpoint, spent_outpoint);
    assert_eq!(
        resolved.spending_transaction_id,
        spend.spending_transaction_id
    );
    assert_eq!(resolved.input_index, spend.input_index);
    assert_eq!(resolved.spending_block_height, spend.block_height);
    assert_eq!(resolved.spending_block_hash, spend.block_hash);
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_returns_no_entry_for_unspent_outpoint() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (_spent_outpoint, _spend) = commit_spent_outpoint_fixture(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let unspent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x9A; 32]), 7);
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![unspent_outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.spends.is_empty(),
        "an unspent outpoint must produce no spend entry",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_dedupes_repeated_request_outpoints() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (spent_outpoint, _spend) = commit_spent_outpoint_fixture(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_spends_by_outpoint(
            vec![spent_outpoint, spent_outpoint, spent_outpoint],
            None::<ChainEpochId>,
        )
        .await?;

    assert_eq!(
        response.spends.len(),
        1,
        "repeated outpoints collapse to one keyed entry",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_grpc_rejects_coinbase_sentinel() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let request = Request::new(wallet::TransparentSpendsByOutpointRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: "00".repeat(32),
            output_index: u32::MAX,
        }],
        at_epoch_id: None,
    });
    let outcome = grpc_adapter.transparent_spends_by_outpoint(request).await;
    let status = match outcome {
        Ok(response) => {
            return Err(eyre!(
                "expected coinbase sentinel rejection, got {response:?}"
            ));
        }
        Err(status) => status,
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("coinbase sentinel"));
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_grpc_projects_block_location() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (spent_outpoint, spend) = commit_spent_outpoint_fixture(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let request = Request::new(wallet::TransparentSpendsByOutpointRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: zinder_core::wire::encode_rpc_transaction_id_hex(
                spent_outpoint.transaction_id,
            ),
            output_index: spent_outpoint.output_index,
        }],
        at_epoch_id: None,
    });
    let response = grpc_adapter
        .transparent_spends_by_outpoint(request)
        .await?
        .into_inner();

    assert!(
        response.chain_view.is_some(),
        "every read carries ChainView"
    );
    assert_eq!(response.spends.len(), 1);
    let wire_spend = response
        .spends
        .first()
        .ok_or_else(|| eyre!("expected one wire spend"))?;
    assert_eq!(
        wire_spend.spending_transaction_id,
        zinder_core::wire::encode_rpc_transaction_id_hex(spend.spending_transaction_id),
    );
    assert_eq!(wire_spend.input_index, spend.input_index);
    let spending_block = wire_spend
        .spending_block
        .as_ref()
        .ok_or_else(|| eyre!("wire spend is missing its spending block"))?;
    assert_eq!(spending_block.height, spend.block_height.value());
    assert_eq!(
        spending_block.hash,
        zinder_core::wire::encode_rpc_block_hash_hex(spend.block_hash),
    );
    Ok(())
}
