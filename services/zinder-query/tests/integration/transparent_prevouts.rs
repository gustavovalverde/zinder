#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use tonic::{Code, Request};
use zinder_core::{
    BlockHeight, ChainEpoch, TransactionArtifact, TransactionId, TransparentOutPoint,
};
use zinder_proto::v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter};
use zinder_store::ChainEpochArtifacts;
use zinder_testkit::{
    P2pkhSpendArgs, StoreFixture, TransparentAddress as TestkitTransparentAddress,
    TransparentTestKey,
};

use crate::common::synthetic_chain_epoch;

const FIXTURE_SEED: [u8; 32] = [
    0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x10,
    0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
];

fn synthetic_p2pkh_transaction(
    block_height: BlockHeight,
    block_hash: zinder_core::BlockHash,
    transaction_id_seed: u8,
    recipient_byte: u8,
    target_height: u32,
) -> eyre::Result<TransactionArtifact> {
    let key = TransparentTestKey::from_seed(&FIXTURE_SEED)?;
    let recipient = TestkitTransparentAddress::PublicKeyHash([recipient_byte; 20]);
    let raw_bytes = key.build_p2pkh_spend(&P2pkhSpendArgs {
        coinbase_txid_be: [0xAA; 32],
        coinbase_vout: 0,
        coinbase_value_zats: 10_000_000,
        recipient: &recipient,
        target_height,
    })?;
    Ok(TransactionArtifact::new(
        TransactionId::from_bytes([transaction_id_seed; 32]),
        block_height,
        block_hash,
        raw_bytes,
    ))
}

#[tokio::test]
async fn transparent_prevouts_resolves_known_outpoint() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let transaction = synthetic_p2pkh_transaction(block.height, block.block_hash, 0xCC, 0x77, 120)?;
    let transaction_id = transaction.transaction_id;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transactions(vec![transaction]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let response = wallet_query
        .transparent_prevouts(
            vec![TransparentOutPoint::new(transaction_id, 0)],
            None::<ChainEpoch>,
        )
        .await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert_eq!(response.entries.len(), 1);
    let prevout = response.entries[0]
        .prevout
        .as_ref()
        .ok_or_else(|| eyre!("expected resolved prevout for indexed transaction"))?;
    assert!(prevout.value_zat > 0, "P2PKH spend should produce a value");
    assert!(
        !prevout.script_pub_key.is_empty(),
        "P2PKH spend should produce a non-empty scriptPubKey",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_prevouts_returns_none_for_unknown_transaction() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);

    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;

    let wallet_query = WalletQuery::new(store, ());
    let response = wallet_query
        .transparent_prevouts(
            vec![TransparentOutPoint::new(
                TransactionId::from_bytes([0xFE; 32]),
                0,
            )],
            None::<ChainEpoch>,
        )
        .await?;

    assert_eq!(response.entries.len(), 1);
    assert!(
        response.entries[0].prevout.is_none(),
        "unknown txid should resolve to None",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_prevouts_returns_none_for_out_of_bounds_index() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let transaction = synthetic_p2pkh_transaction(block.height, block.block_hash, 0xAB, 0x33, 120)?;
    let transaction_id = transaction.transaction_id;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transactions(vec![transaction]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let response = wallet_query
        .transparent_prevouts(
            vec![TransparentOutPoint::new(transaction_id, 99)],
            None::<ChainEpoch>,
        )
        .await?;

    assert_eq!(response.entries.len(), 1);
    assert!(
        response.entries[0].prevout.is_none(),
        "out-of-bounds output_index should resolve to None",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_mempool_prevouts_grpc_rejects_coinbase_sentinel() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    let wallet_query = WalletQuery::new(store, ());
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let request = Request::new(wallet::TransparentMempoolPrevoutsRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: vec![0u8; 32],
            output_index: u32::MAX,
        }],
    });
    let outcome = grpc_adapter.transparent_mempool_prevouts(request).await;
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
async fn transparent_prevouts_grpc_rejects_coinbase_sentinel() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    store.commit_chain_epoch(ChainEpochArtifacts::new(
        chain_epoch,
        vec![block],
        vec![compact_block],
    ))?;
    let wallet_query = WalletQuery::new(store, ());
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());

    let request = Request::new(wallet::TransparentPrevoutsRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: vec![0u8; 32],
            output_index: u32::MAX,
        }],
        at_epoch: None,
    });
    let outcome = grpc_adapter.transparent_prevouts(request).await;
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
async fn transparent_prevouts_preserves_input_order_and_dedupes_reads() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let transaction = synthetic_p2pkh_transaction(block.height, block.block_hash, 0xCC, 0x55, 120)?;
    let transaction_id = transaction.transaction_id;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transactions(vec![transaction]),
    )?;

    let wallet_query = WalletQuery::new(store, ());
    let outpoints = vec![
        TransparentOutPoint::new(transaction_id, 0),
        TransparentOutPoint::new(TransactionId::from_bytes([0xEE; 32]), 0),
        TransparentOutPoint::new(transaction_id, 0),
    ];
    let response = wallet_query
        .transparent_prevouts(outpoints.clone(), None::<ChainEpoch>)
        .await?;

    assert_eq!(response.entries.len(), 3);
    assert_eq!(response.entries[0].outpoint, outpoints[0]);
    assert_eq!(response.entries[1].outpoint, outpoints[1]);
    assert_eq!(response.entries[2].outpoint, outpoints[2]);
    assert!(response.entries[0].prevout.is_some());
    assert!(response.entries[1].prevout.is_none());
    assert_eq!(response.entries[0].prevout, response.entries[2].prevout);
    Ok(())
}
