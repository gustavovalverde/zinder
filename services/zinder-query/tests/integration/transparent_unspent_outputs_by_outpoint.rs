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
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::{chain_epoch_artifacts_with_transparent_facts, synthetic_chain_epoch};

/// Commits one unspent output in the first epoch and a second output that a
/// later epoch spends.
///
/// Returns the unspent outpoint, the spent outpoint, and the spent output's
/// value so the probe can assert that only the unspent one is returned, and
/// with the correct payload.
fn commit_unspent_and_spent_outputs(
    store: &zinder_store::PrimaryChainStore,
) -> eyre::Result<(TransparentOutPoint, TransparentOutPoint, u64)> {
    let (epoch_one, block_one, compact_one) = synthetic_chain_epoch(1, 1);
    let (epoch_two, block_two, compact_two) = synthetic_chain_epoch(2, 2);

    let unspent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x21; 32]), 0);
    let unspent_script = vec![0x76, 0xa9, 0x14, 0x11, 0xac];
    let unspent_value_zat = 7_000_000;
    let unspent_output = TransparentOutputArtifact::new(
        unspent_outpoint,
        unspent_value_zat,
        unspent_script.clone(),
        TransparentAddressScriptHash::of_script_pub_key(&unspent_script),
        block_one.height,
        block_one.block_hash,
    );

    let spent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x31; 32]), 0);
    let spent_script = vec![0x76, 0xa9, 0x14, 0x22, 0xac];
    let spent_output = TransparentOutputArtifact::new(
        spent_outpoint,
        12_345_678,
        spent_script.clone(),
        TransparentAddressScriptHash::of_script_pub_key(&spent_script),
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
        spent_output.value_zat,
        spent_output.address_script_hash,
        spent_output.block_height,
        spent_output.block_hash,
    );
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_one,
        vec![block_one],
        vec![compact_one],
        &[unspent_output, spent_output],
        Vec::new(),
    ))?;
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_two,
        vec![block_two],
        vec![compact_two],
        &[],
        vec![spend],
    ))?;

    Ok((unspent_outpoint, spent_outpoint, unspent_value_zat))
}

#[tokio::test]
async fn transparent_unspent_outputs_by_outpoint_returns_an_unspent_output() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (unspent_outpoint, _spent_outpoint, unspent_value_zat) =
        commit_unspent_and_spent_outputs(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_unspent_outputs_by_outpoint(vec![unspent_outpoint], None::<ChainEpochId>)
        .await?;

    assert_eq!(response.entries.len(), 1);
    let entry = response
        .entries
        .first()
        .ok_or_else(|| eyre!("expected one unspent entry"))?;
    assert_eq!(entry.outpoint, unspent_outpoint);
    let output = entry
        .output
        .as_ref()
        .ok_or_else(|| eyre!("an unspent entry must carry its output"))?;
    assert_eq!(output.value_zat, unspent_value_zat);
    Ok(())
}

#[tokio::test]
async fn transparent_unspent_outputs_by_outpoint_omits_a_spent_output() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (_unspent_outpoint, spent_outpoint, _unspent_value_zat) =
        commit_unspent_and_spent_outputs(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_unspent_outputs_by_outpoint(vec![spent_outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.entries.is_empty(),
        "a spent outpoint must produce no entry (null-if-spent)",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_unspent_outputs_by_outpoint_omits_a_never_existed_outpoint() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (_unspent_outpoint, _spent_outpoint, _unspent_value_zat) =
        commit_unspent_and_spent_outputs(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let absent_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x9A; 32]), 7);
    let response = wallet_query
        .transparent_unspent_outputs_by_outpoint(vec![absent_outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.entries.is_empty(),
        "an outpoint the canonical chain never had must produce no entry",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_unspent_outputs_by_outpoint_dedupes_repeated_request_outpoints()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (unspent_outpoint, _spent_outpoint, _unspent_value_zat) =
        commit_unspent_and_spent_outputs(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_unspent_outputs_by_outpoint(
            vec![unspent_outpoint, unspent_outpoint, unspent_outpoint],
            None::<ChainEpochId>,
        )
        .await?;

    assert_eq!(
        response.entries.len(),
        1,
        "repeated outpoints collapse to one keyed entry",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_unspent_outputs_by_outpoint_grpc_rejects_coinbase_sentinel() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        chain_epoch,
        vec![block],
        vec![compact_block],
        &[],
        Vec::new(),
    ))?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let request = Request::new(wallet::TransparentUnspentOutputsByOutpointRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: "00".repeat(32),
            output_index: u32::MAX,
        }],
        at_epoch_id: None,
    });
    let outcome = grpc_adapter
        .transparent_unspent_outputs_by_outpoint(request)
        .await;
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
async fn transparent_unspent_outputs_by_outpoint_grpc_carries_chain_view() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (unspent_outpoint, _spent_outpoint, unspent_value_zat) =
        commit_unspent_and_spent_outputs(&store)?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let request = Request::new(wallet::TransparentUnspentOutputsByOutpointRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: zinder_core::wire::encode_rpc_transaction_id_hex(
                unspent_outpoint.transaction_id,
            ),
            output_index: unspent_outpoint.output_index,
        }],
        at_epoch_id: None,
    });
    let response = grpc_adapter
        .transparent_unspent_outputs_by_outpoint(request)
        .await?
        .into_inner();

    assert!(
        response.chain_view.is_some(),
        "every read carries ChainView"
    );
    assert_eq!(response.entries.len(), 1);
    let entry = response
        .entries
        .first()
        .ok_or_else(|| eyre!("expected one wire entry"))?;
    let output = entry
        .output
        .as_ref()
        .ok_or_else(|| eyre!("wire entry is missing its output"))?;
    assert_eq!(output.value_zat, unspent_value_zat);
    Ok(())
}
