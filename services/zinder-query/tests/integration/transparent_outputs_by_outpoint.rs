#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use eyre::eyre;
use std::sync::Arc;
use tonic::{Code, Request};
use zinder_core::{
    BlockHash, BlockHeight, BlockHeightRange, ChainEpoch, TransactionId,
    TransparentAddressScriptHash, TransparentOutPoint, TransparentOutputArtifact,
};
use zinder_proto::v1::wallet::{self, wallet_query_server::WalletQuery as WalletQueryService};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryApi, WalletQueryGrpcAdapter};
use zinder_store::{ChainEpochArtifacts, ReorgWindowChange};
use zinder_testkit::{StoreFixture, sample_regtest_upgrade_activations};

use crate::common::synthetic_chain_epoch;

fn synthetic_transparent_output_artifact(
    block_height: BlockHeight,
    block_hash: BlockHash,
    transaction_id_seed: u8,
    script_seed: u8,
) -> TransparentOutputArtifact {
    let outpoint =
        TransparentOutPoint::new(TransactionId::from_bytes([transaction_id_seed; 32]), 0);
    let script_pub_key = vec![0x76, 0xa9, script_seed, 0x88, 0xac];
    let address_script_hash = TransparentAddressScriptHash::of_script_pub_key(&script_pub_key);
    TransparentOutputArtifact::new(
        outpoint,
        10_000_000 + u64::from(script_seed),
        script_pub_key,
        address_script_hash,
        block_height,
        block_hash,
    )
}

#[tokio::test]
async fn transparent_outputs_by_outpoint_resolves_known_outpoint() -> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let prevout = synthetic_transparent_output_artifact(block.height, block.block_hash, 0xCC, 0x77);
    let outpoint = prevout.outpoint;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transparent_outputs_by_outpoint(vec![prevout]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_outputs_by_outpoint(vec![outpoint], None::<ChainEpoch>)
        .await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert_eq!(response.entries.len(), 1);
    let prevout = response.entries[0]
        .output
        .as_ref()
        .ok_or_else(|| eyre!("expected resolved indexed transparent output"))?;
    assert!(prevout.value_zat > 0, "prevout should carry a value");
    assert!(
        !prevout.script_pub_key.is_empty(),
        "prevout should carry a non-empty scriptPubKey",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_outputs_by_outpoint_returns_none_for_unknown_transaction() -> eyre::Result<()>
{
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
        .transparent_outputs_by_outpoint(
            vec![TransparentOutPoint::new(
                TransactionId::from_bytes([0xFE; 32]),
                0,
            )],
            None::<ChainEpoch>,
        )
        .await?;

    assert_eq!(response.entries.len(), 1);
    assert!(
        response.entries[0].output.is_none(),
        "unknown txid should resolve to None",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_outputs_by_outpoint_returns_none_for_out_of_bounds_index() -> eyre::Result<()>
{
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let prevout = synthetic_transparent_output_artifact(block.height, block.block_hash, 0xAB, 0x33);
    let transaction_id = prevout.outpoint.transaction_id;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transparent_outputs_by_outpoint(vec![prevout]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let response = wallet_query
        .transparent_outputs_by_outpoint(
            vec![TransparentOutPoint::new(transaction_id, 99)],
            None::<ChainEpoch>,
        )
        .await?;

    assert_eq!(response.entries.len(), 1);
    assert!(
        response.entries[0].output.is_none(),
        "out-of-bounds output_index should resolve to None",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_mempool_outputs_by_outpoint_grpc_rejects_coinbase_sentinel() -> eyre::Result<()>
{
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

    let request = Request::new(wallet::TransparentMempoolOutputsByOutpointRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: "00".repeat(32),
            output_index: u32::MAX,
        }],
    });
    let outcome = grpc_adapter
        .transparent_mempool_outputs_by_outpoint(request)
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
async fn transparent_outputs_by_outpoint_grpc_rejects_coinbase_sentinel() -> eyre::Result<()> {
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

    let request = Request::new(wallet::TransparentOutputsByOutpointRequest {
        outpoints: vec![wallet::OutPoint {
            transaction_id: "00".repeat(32),
            output_index: u32::MAX,
        }],
        at_epoch: None,
    });
    let outcome = grpc_adapter.transparent_outputs_by_outpoint(request).await;
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

/// Fixture: two committed blocks at distinct heights with three transactions
/// (two in the first block, one in the second). Returns the chain epoch and
/// the three transaction ids in commit order.
fn commit_two_block_fixture(
    store: &zinder_store::PrimaryChainStore,
) -> eyre::Result<(ChainEpoch, [TransactionId; 3])> {
    use zinder_core::{
        ArtifactSchemaVersion, BlockHash, BlockHeaderArtifact, ChainEpochId, ChainTipMetadata,
        CompactBlockArtifact, Network, UnixTimestampMillis,
    };

    let first_height = BlockHeight::new(100);
    let second_height = BlockHeight::new(101);
    let first_hash = BlockHash::from_bytes([0x11; 32]);
    let second_hash = BlockHash::from_bytes([0x22; 32]);

    let chain_epoch = ChainEpoch {
        id: ChainEpochId::new(1),
        network: Network::ZcashRegtest,
        tip_height: second_height,
        tip_hash: second_hash,
        safe_tip_height: second_height,
        safe_tip_hash: second_hash,
        artifact_schema_version: ArtifactSchemaVersion::new(11),
        tip_metadata: ChainTipMetadata::empty(),
        created_at: UnixTimestampMillis::new(1_774_668_300_000),
    };

    let blocks = vec![
        BlockHeaderArtifact::new(
            first_height,
            first_hash,
            BlockHash::from_bytes([0x10; 32]),
            [0; 32],
            [0; 32],
            0,
            0,
            [0; 32],
            0,
            u64::try_from(b"raw-block-1-100".len()).unwrap_or(u64::MAX),
        ),
        BlockHeaderArtifact::new(
            second_height,
            second_hash,
            first_hash,
            [0; 32],
            [0; 32],
            0,
            0,
            [0; 32],
            0,
            u64::try_from(b"raw-block-1-101".len()).unwrap_or(u64::MAX),
        ),
    ];
    let compact_blocks = vec![
        CompactBlockArtifact::new(first_height, first_hash, b"compact-block-1-100".to_vec()),
        CompactBlockArtifact::new(second_height, second_hash, b"compact-block-1-101".to_vec()),
    ];
    let prevouts = vec![
        synthetic_transparent_output_artifact(first_height, first_hash, 0xA1, 0x11),
        synthetic_transparent_output_artifact(first_height, first_hash, 0xA2, 0x22),
        synthetic_transparent_output_artifact(second_height, second_hash, 0xB1, 0x33),
    ];
    let ids = [
        prevouts[0].outpoint.transaction_id,
        prevouts[1].outpoint.transaction_id,
        prevouts[2].outpoint.transaction_id,
    ];
    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, blocks, compact_blocks)
            .with_transparent_outputs_by_outpoint(prevouts)
            .with_reorg_window_change(ReorgWindowChange::Extend {
                block_range: BlockHeightRange::inclusive(first_height, second_height),
            }),
    )?;
    Ok((chain_epoch, ids))
}

#[tokio::test]
async fn transparent_outputs_by_outpoint_resolves_outpoints_across_multiple_blocks()
-> eyre::Result<()> {
    // Three transactions spread across two distinct heights, six outpoints
    // (including a repeated one and an unknown one) in mixed order. Exercises
    // direct indexed prevout lookup across visible blocks while preserving
    // per-outpoint input order.
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, [txid_first_block_a, txid_first_block_b, txid_second_block]) =
        commit_two_block_fixture(&store)?;
    let unknown_transaction_id = TransactionId::from_bytes([0xEE; 32]);

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let outpoints = vec![
        TransparentOutPoint::new(txid_second_block, 0),
        TransparentOutPoint::new(txid_first_block_a, 0),
        TransparentOutPoint::new(unknown_transaction_id, 0),
        TransparentOutPoint::new(txid_first_block_b, 0),
        TransparentOutPoint::new(txid_first_block_a, 0),
        TransparentOutPoint::new(txid_second_block, 0),
    ];
    let response = wallet_query
        .transparent_outputs_by_outpoint(outpoints.clone(), None::<ChainEpoch>)
        .await?;

    assert_eq!(response.chain_epoch, chain_epoch);
    assert_eq!(response.entries.len(), outpoints.len());
    for (index, entry) in response.entries.iter().enumerate() {
        assert_eq!(entry.outpoint, outpoints[index]);
    }
    assert!(
        response.entries[0].output.is_some(),
        "second-block txn resolves"
    );
    assert!(
        response.entries[1].output.is_some(),
        "first-block txn A resolves"
    );
    assert!(
        response.entries[2].output.is_none(),
        "unknown txid resolves to None",
    );
    assert!(
        response.entries[3].output.is_some(),
        "first-block txn B resolves"
    );
    assert_eq!(
        response.entries[1].output, response.entries[4].output,
        "repeated first-block txn A returns identical prevout",
    );
    assert_eq!(
        response.entries[0].output, response.entries[5].output,
        "repeated second-block txn returns identical prevout",
    );
    Ok(())
}

#[tokio::test]
async fn transparent_outputs_by_outpoint_preserves_input_order_and_dedupes_reads()
-> eyre::Result<()> {
    let store_fixture = StoreFixture::open()?;
    let store = store_fixture.chain_store().clone();
    let (chain_epoch, block, compact_block) = synthetic_chain_epoch(1, 1);
    let prevout = synthetic_transparent_output_artifact(block.height, block.block_hash, 0xCC, 0x55);
    let transaction_id = prevout.outpoint.transaction_id;

    store.commit_chain_epoch(
        ChainEpochArtifacts::new(chain_epoch, vec![block], vec![compact_block])
            .with_transparent_outputs_by_outpoint(vec![prevout]),
    )?;

    let wallet_query = WalletQuery::new(store, (), Arc::new(sample_regtest_upgrade_activations()));
    let outpoints = vec![
        TransparentOutPoint::new(transaction_id, 0),
        TransparentOutPoint::new(TransactionId::from_bytes([0xEE; 32]), 0),
        TransparentOutPoint::new(transaction_id, 0),
    ];
    let response = wallet_query
        .transparent_outputs_by_outpoint(outpoints.clone(), None::<ChainEpoch>)
        .await?;

    assert_eq!(response.entries.len(), 3);
    assert_eq!(response.entries[0].outpoint, outpoints[0]);
    assert_eq!(response.entries[1].outpoint, outpoints[1]);
    assert_eq!(response.entries[2].outpoint, outpoints[2]);
    assert!(response.entries[0].output.is_some());
    assert!(response.entries[1].output.is_none());
    assert_eq!(response.entries[0].output, response.entries[2].output);
    Ok(())
}
