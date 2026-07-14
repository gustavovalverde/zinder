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
use zinder_derive::{DeriveStore, DeriveStoreOptions, ProjectionPreset};
use zinder_proto::v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService};
use zinder_query::{
    QueryError, ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter,
};
use zinder_store::{ChainEpochArtifacts, ReorgWindowChange};
use zinder_testkit::{
    StoreFixture, open_test_derive_store_for_canonical, sample_regtest_upgrade_activations,
    seed_transparent_outpoint_spends,
};

use crate::common::{block_hash_from_seed, synthetic_chain_epoch, synthetic_multi_block_epoch};

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
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_one,
        vec![block_one],
        vec![compact_one],
    ))?;
    let (epoch_two, block_two, compact_two) = synthetic_chain_epoch(2, 2);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        epoch_two,
        vec![block_two],
        vec![compact_two],
    ))?;
    Ok(())
}

/// Builds a spend fact that exists only in the derive projection (its outpoint
/// is never committed to the canonical store).
fn derive_only_spend(outpoint: TransparentOutPoint, height: BlockHeight) -> TransparentSpendFact {
    TransparentSpendFact::new(
        outpoint,
        3,
        TransactionId::from_bytes([0x55; 32]),
        0,
        height,
        block_hash_from_seed(height.value()),
        1_000,
        TransparentAddressScriptHash::from_bytes([0x66; 32]),
        BlockHeight::new(1),
        block_hash_from_seed(1),
    )
}

fn open_wallet_derive_store(canonical_path: &std::path::Path) -> eyre::Result<DeriveStore> {
    Ok(DeriveStore::open_with_projection_preset(
        DeriveStore::path_for_canonical(canonical_path),
        ProjectionPreset::Wallet,
        DeriveStoreOptions {
            rocksdb_resource_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
            ..DeriveStoreOptions::default()
        },
    )?)
}

#[tokio::test]
async fn transparent_spends_by_outpoint_resolves_swept_spend_from_derive() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let derive_store = open_wallet_derive_store(store_fixture.tempdir_path())?;
    commit_to_settled_tip_two(&store)?;

    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x41; 32]), 0);
    let spend = derive_only_spend(outpoint, BlockHeight::new(1));
    seed_transparent_outpoint_spends(&derive_store, std::slice::from_ref(&spend))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_derive_store(derive_store);
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![outpoint], None::<ChainEpochId>)
        .await?;

    assert_eq!(response.spends.len(), 1);
    let resolved = response
        .spends
        .first()
        .ok_or_else(|| eyre!("expected the derive projection to resolve the spend"))?;
    assert_eq!(resolved.spent_outpoint, outpoint);
    assert_eq!(
        resolved.spending_transaction_id,
        spend.spending_transaction_id
    );
    assert_eq!(resolved.spending_block_height, spend.block_height);
    assert_eq!(resolved.input_index, spend.input_index);
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_ignores_derive_spend_above_settled_tip() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    commit_to_settled_tip_two(&store)?;

    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x42; 32]), 0);
    let spend = derive_only_spend(outpoint, BlockHeight::new(5));
    seed_transparent_outpoint_spends(&derive_store, &[spend])?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_derive_store(derive_store);
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.spends.is_empty(),
        "a derive spend above the settled tip keeps the in-window absent semantics",
    );
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
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(epoch_one, blocks, compact_blocks)
            .with_transparent_outputs_by_outpoint(vec![output])
            .with_transparent_spend_facts(vec![spend]),
    )?;

    store.set_transparent_retention_release_height(BlockHeight::new(3))?;
    let sweep_epoch = synthetic_multi_block_epoch(2, 3, 2).0;
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(sweep_epoch, Vec::new(), Vec::new()).with_reorg_window_change(
            ReorgWindowChange::AdvanceSafeTipTo {
                height: BlockHeight::new(2),
            },
        ),
    )?;
    let sweep = store.sweep_transparent_retention_once()?;
    assert_eq!(sweep.swept_heights(), 2);
    assert_eq!(sweep.swept_outpoints(), 1);
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_refuses_when_derive_trails_the_sweep() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    commit_real_sweep_to_deleted_through_two(&store)?;

    let missing_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x43; 32]), 0);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_derive_store(derive_store);
    let outcome = wallet_query
        .transparent_spends_by_outpoint(vec![missing_outpoint], None::<ChainEpochId>)
        .await;

    match outcome {
        Err(QueryError::DeriveLag { derive_height, .. }) => {
            assert_eq!(derive_height, None);
        }
        other => return Err(eyre!("expected a derive-lag refusal, got {other:?}")),
    }
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
async fn transparent_spends_by_outpoint_returns_absent_when_never_swept_and_derive_empty()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    // No sweep ran, so nothing was deleted and the deleted-through marker is
    // unset. An empty derive projection must not turn a canonical miss into a
    // lag refusal; the honest answer is absent.
    commit_to_settled_tip_two(&store)?;

    let missing_outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x44; 32]), 0);
    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_derive_store(derive_store);
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![missing_outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.spends.is_empty(),
        "an empty projection over a store that never swept must read as absent",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_skips_reorged_out_derive_row() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    commit_to_settled_tip_two(&store)?;

    // A projection row whose spending block hash names a branch the canonical
    // header at that height no longer carries: a stale in-window row left by a
    // reorg the tailer has not yet replayed. It must not surface as the spender.
    let outpoint = TransparentOutPoint::new(TransactionId::from_bytes([0x45; 32]), 0);
    let mut stale = derive_only_spend(outpoint, BlockHeight::new(1));
    stale.block_hash = block_hash_from_seed(999);
    seed_transparent_outpoint_spends(&derive_store, std::slice::from_ref(&stale))?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_derive_store(derive_store);
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![outpoint], None::<ChainEpochId>)
        .await?;

    assert!(
        response.spends.is_empty(),
        "a reorged-out projection row must be skipped, not served as the spender",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_spends_by_outpoint_prefers_the_canonical_spender() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let derive_store = open_test_derive_store_for_canonical(store_fixture.tempdir_path())?;
    let (spent_outpoint, spend) = commit_spent_outpoint_fixture(&store)?;

    // Seed the derive projection with a different spender for the same outpoint;
    // the canonical read must win when it hits.
    let conflicting = TransparentSpendFact::new(
        spent_outpoint,
        9,
        TransactionId::from_bytes([0xEE; 32]),
        0,
        spend.block_height,
        spend.block_hash,
        spend.spent_value_zat,
        spend.spent_address_script_hash,
        spend.spent_block_height,
        spend.spent_block_hash,
    );
    seed_transparent_outpoint_spends(&derive_store, &[conflicting])?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()))
        .with_derive_store(derive_store);
    let response = wallet_query
        .transparent_spends_by_outpoint(vec![spent_outpoint], None::<ChainEpochId>)
        .await?;

    assert_eq!(response.spends.len(), 1);
    let resolved = response
        .spends
        .first()
        .ok_or_else(|| eyre!("expected the canonical spend"))?;
    assert_eq!(
        resolved.spending_transaction_id,
        spend.spending_transaction_id
    );
    assert_eq!(resolved.input_index, spend.input_index);
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
