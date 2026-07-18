#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::sync::Arc;

use eyre::eyre;
use tonic::{Code, Request};
use zinder_core::{
    BlockHeight, ChainEpochId, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentOutputArtifact, TransparentSpendFact,
};
use zinder_proto::v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService};
use zinder_query::{
    QueryError, ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter,
};
use zinder_store::{ChainEpochArtifacts, ReorgWindowChange};
use zinder_testkit::{
    StoreFixture, encode_fixture_block_replay, sample_regtest_upgrade_activations,
};

use crate::common::{
    block_hash_from_seed, chain_epoch_artifacts_with_transparent_facts, synthetic_chain_epoch,
    synthetic_multi_block_epoch,
};

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
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_one,
        vec![block_one],
        vec![compact_one],
        &[output],
        Vec::new(),
    ))?;
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_two,
        vec![block_two],
        vec![compact_two],
        &[],
        vec![spend.clone()],
    ))?;

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
async fn transparent_spends_by_outpoint_respects_the_pinned_epoch() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (spent_outpoint, spend) = commit_spent_outpoint_fixture(&store)?;
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));

    let before_spend = wallet_query
        .transparent_spends_by_outpoint(vec![spent_outpoint], Some(ChainEpochId::new(1)))
        .await?;
    let after_spend = wallet_query
        .transparent_spends_by_outpoint(vec![spent_outpoint], Some(ChainEpochId::new(2)))
        .await?;

    assert!(before_spend.spends.is_empty());
    assert_eq!(after_spend.spends.len(), 1);
    assert_eq!(
        after_spend.spends[0].spending_transaction_id,
        spend.spending_transaction_id
    );
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

/// Commits two plain epochs so the canonical settled tip reaches height 2
/// without running a retention sweep (the swept marker stays at zero).
fn commit_to_settled_tip_two(store: &zinder_store::PrimaryChainStore) -> eyre::Result<()> {
    let (epoch_one, block_one, compact_one) = synthetic_chain_epoch(1, 1);
    let replay_one = encode_fixture_block_replay(&block_one, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_one,
        vec![block_one],
        vec![replay_one],
        vec![compact_one],
    ))?;
    let (epoch_two, block_two, compact_two) = synthetic_chain_epoch(2, 2);
    let replay_two = encode_fixture_block_replay(&block_two, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_two,
        vec![block_two],
        vec![replay_two],
        vec![compact_two],
    ))?;
    Ok(())
}

/// Commits an output, settles its spend, advances the safe tip, and explicitly
/// runs retention maintenance so the deleted-through marker reaches height 2.
fn commit_real_sweep_to_deleted_through_two(
    store: &zinder_store::PrimaryChainStore,
) -> eyre::Result<()> {
    let (epoch_one, blocks, compact_blocks) = synthetic_multi_block_epoch(1, 3, 1);
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x31; 32]), 0);
    let script_pub_key = vec![0x76, 0xa9, 0x14, 0x88, 0xac];
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    let output = TransparentOutputArtifact::new(
        outpoint,
        1_000,
        script_pub_key,
        address_script_hash,
        BlockHeight::new(1),
        block_hash_from_seed(1),
    );
    let spend = TransparentSpendFact::new(
        outpoint,
        2,
        TransactionId::from_bytes([0x33; 32]),
        0,
        BlockHeight::new(2),
        block_hash_from_seed(2),
        output.value_zat,
        output.address_script_hash,
        output.block_height,
        output.block_hash,
    );
    store.commit_chain_epoch(chain_epoch_artifacts_with_transparent_facts(
        epoch_one,
        blocks,
        compact_blocks,
        &[output],
        vec![spend],
    ))?;

    store.set_transparent_retention_release_height(BlockHeight::new(3))?;
    let sweep_epoch = synthetic_multi_block_epoch(2, 3, 2).0;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(sweep_epoch, Vec::new(), Vec::new(), Vec::new())
            .with_reorg_window_change(ReorgWindowChange::AdvanceSafeTipTo {
                height: BlockHeight::new(2),
            }),
    )?;
    let sweep = store.sweep_transparent_retention_once()?;
    assert_eq!(sweep.swept_heights(), 2);
    assert_eq!(sweep.swept_outpoints(), 1);
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_refuses_swept_miss_without_derive() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    commit_real_sweep_to_deleted_through_two(&store)?;

    let missing_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x45; 32]), 0);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let outcome = wallet_query
        .transparent_spends_by_outpoint(vec![missing_outpoint], None::<ChainEpochId>)
        .await;

    assert!(matches!(
        outcome,
        Err(QueryError::DeriveUnavailable {
            capability: zinder_proto::capabilities::WALLET_READ_TRANSPARENT_SPENDS_V1,
        })
    ));
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_returns_absent_when_never_swept() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    // No sweep ran, so nothing was deleted and the deleted-through marker is
    // unset. A canonical miss must read as absent rather than a lag refusal.
    commit_to_settled_tip_two(&store)?;

    let missing_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x44; 32]), 0);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![missing_outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.spends.is_empty(),
        "a canonical miss over a store that never swept must read as absent",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_grpc_rejects_coinbase_sentinel() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let replay = encode_fixture_block_replay(&block, &[]);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![replay],
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
