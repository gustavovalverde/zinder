#![allow(
    missing_docs,
    reason = "Integration test names describe the compatibility behavior under test."
)]

use eyre::eyre;
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Code, Request, transport::Server};
use zebra_chain::{
    parameters::NetworkKind as ZebraNetworkKind, transparent::Address as ZebraTransparentAddress,
};
use zinder_compat_lightwalletd::LightwalletdGrpcAdapter;
use zinder_core::{
    BlockHash, BlockHeight, BroadcastDuplicate, BroadcastInvalidEncoding, BroadcastRejected,
    BroadcastUnknown, ChainEpochId, ChainTipMetadata, Network, SUBTREE_LEAF_COUNT,
    ShieldedProtocol, SubtreeRootArtifact, SubtreeRootHash, SubtreeRootIndex, TransactionArtifact,
    TransactionBroadcastResult, TransactionId, TransparentAddressScriptHash,
    TransparentAddressUtxoArtifact, TransparentOutPoint, TransparentUtxoSpendArtifact,
};
use zinder_proto::compat::lightwalletd::{
    self, compact_tx_streamer_client::CompactTxStreamerClient,
    compact_tx_streamer_server::CompactTxStreamer,
};
use zinder_query::WalletQuery;
use zinder_testkit::{ChainFixture, MockTransactionBroadcaster, StoreFixture};

const ACCEPTANCE_BLOCK_HEIGHT: BlockHeight = BlockHeight::new(1);
const SAPLING_SUBTREE_ROOT_HASH: [u8; 32] = [7; 32];
const DEFAULT_TREE_STATE_PAYLOAD: &[u8] =
    br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":{"finalState":"000000"}},"orchard":{"commitments":{}}}"#;

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "M1 acceptance test keeps the read-sync RPC matrix together."
)]
async fn lightwalletd_adapter_serves_m1_read_sync_methods() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let latest_block = adapter
        .get_latest_block(Request::new(lightwalletd::ChainSpec {}))
        .await?
        .into_inner();
    let block = adapter
        .get_block(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();
    let block_range = adapter
        .get_block_range(Request::new(lightwalletd::BlockRange {
            start: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            end: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            pool_types: Vec::new(),
        }))
        .await?
        .into_inner();
    let ranged_blocks = collect_stream(block_range).await?;
    let tree_state = adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();
    let latest_tree_state = adapter
        .get_latest_tree_state(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();
    let subtree_roots = adapter
        .get_subtree_roots(Request::new(lightwalletd::GetSubtreeRootsArg {
            start_index: 0,
            shielded_protocol: lightwalletd::ShieldedProtocol::Sapling as i32,
            max_entries: 1,
        }))
        .await?
        .into_inner();
    let subtree_roots = collect_stream(subtree_roots).await?;
    let lightd_info = adapter
        .get_lightd_info(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();

    assert_eq!(latest_block.height, 1);
    assert_eq!(block.height, 1);
    assert_eq!(block.vtx.len(), 1);
    assert_eq!(ranged_blocks.len(), 1);
    assert_eq!(ranged_blocks[0].height, 1);
    assert_eq!(ranged_blocks[0].vtx[0].vin.len(), 0);
    assert_eq!(ranged_blocks[0].vtx[0].vout.len(), 0);
    assert_eq!(tree_state.sapling_tree, "000000");
    assert_eq!(latest_tree_state.sapling_tree, tree_state.sapling_tree);
    assert_eq!(subtree_roots.len(), 1);
    assert_eq!(subtree_roots[0].completing_block_height, 1);
    assert_eq!(lightd_info.vendor, "Zinder");
    assert_eq!(lightd_info.chain_name, "regtest");
    assert_eq!(lightd_info.block_height, 1);
    assert_eq!(lightd_info.estimated_height, 1);
    assert_eq!(
        lightd_info.lightwallet_protocol_version,
        lightwalletd::LIGHTWALLETD_PROTOCOL_COMMIT
    );

    Ok(())
}

#[tokio::test]
async fn get_address_utxos_stream_returns_indexed_unspent_transparent_outputs() -> eyre::Result<()>
{
    let transparent_address =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x11; 20]);
    let address = transparent_address.to_string();
    let script_pub_key = transparent_address.script().as_raw_bytes().to_vec();
    let transaction_id = TransactionId::from_bytes([0x55; 32]);
    let spent_transaction_id = TransactionId::from_bytes([0x66; 32]);
    let store_fixture = acceptance_store_fixture_with_transactions_and_transparent(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |_| Vec::new(),
        |block| {
            let unspent_outpoint = TransparentOutPoint::new(transaction_id, 0);
            let spent_outpoint = TransparentOutPoint::new(spent_transaction_id, 1);
            (
                vec![
                    TransparentAddressUtxoArtifact::new(
                        transparent_address_script_hash(&script_pub_key),
                        script_pub_key.clone(),
                        unspent_outpoint,
                        12,
                        block.height,
                        block.hash,
                    ),
                    TransparentAddressUtxoArtifact::new(
                        transparent_address_script_hash(&script_pub_key),
                        script_pub_key.clone(),
                        spent_outpoint,
                        13,
                        block.height,
                        block.hash,
                    ),
                ],
                vec![TransparentUtxoSpendArtifact::new(
                    spent_outpoint,
                    block.height,
                    block.hash,
                )],
            )
        },
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let request = lightwalletd::GetAddressUtxosArg {
        addresses: vec![address.clone()],
        start_height: 1,
        max_entries: 10,
    };
    let list_response = adapter
        .get_address_utxos(Request::new(request.clone()))
        .await?
        .into_inner();
    let stream_response = adapter
        .get_address_utxos_stream(Request::new(request))
        .await?
        .into_inner();
    let streamed_utxos = collect_stream(stream_response).await?;
    let lightd_info = adapter
        .get_lightd_info(Request::new(lightwalletd::Empty {}))
        .await?
        .into_inner();

    assert_eq!(list_response.address_utxos, streamed_utxos);
    assert_eq!(streamed_utxos.len(), 1);
    assert_eq!(streamed_utxos[0].address, address);
    assert_eq!(streamed_utxos[0].txid, transaction_id.as_bytes().to_vec());
    assert_eq!(streamed_utxos[0].index, 0);
    assert_eq!(streamed_utxos[0].script, script_pub_key);
    assert_eq!(streamed_utxos[0].value_zat, 12);
    assert_eq!(streamed_utxos[0].height, 1);
    assert!(lightd_info.taddr_support);

    Ok(())
}

#[tokio::test]
async fn get_address_utxos_applies_max_entries_across_address_set() -> eyre::Result<()> {
    let transparent_address_a =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x21; 20]);
    let transparent_address_b =
        ZebraTransparentAddress::from_pub_key_hash(ZebraNetworkKind::Regtest, [0x22; 20]);
    let address_a = transparent_address_a.to_string();
    let address_b = transparent_address_b.to_string();
    let script_pub_key_a = transparent_address_a.script().as_raw_bytes().to_vec();
    let script_pub_key_b = transparent_address_b.script().as_raw_bytes().to_vec();
    let first_transaction_id = TransactionId::from_bytes([0x10; 32]);
    let second_transaction_id = TransactionId::from_bytes([0x20; 32]);
    let truncated_transaction_id = TransactionId::from_bytes([0x30; 32]);
    let store_fixture = acceptance_store_fixture_with_transactions_and_transparent(
        DEFAULT_TREE_STATE_PAYLOAD.to_vec(),
        |_| Vec::new(),
        |block| {
            (
                vec![
                    TransparentAddressUtxoArtifact::new(
                        transparent_address_script_hash(&script_pub_key_a),
                        script_pub_key_a.clone(),
                        TransparentOutPoint::new(truncated_transaction_id, 0),
                        30,
                        block.height,
                        block.hash,
                    ),
                    TransparentAddressUtxoArtifact::new(
                        transparent_address_script_hash(&script_pub_key_b),
                        script_pub_key_b.clone(),
                        TransparentOutPoint::new(first_transaction_id, 0),
                        10,
                        block.height,
                        block.hash,
                    ),
                    TransparentAddressUtxoArtifact::new(
                        transparent_address_script_hash(&script_pub_key_b),
                        script_pub_key_b.clone(),
                        TransparentOutPoint::new(second_transaction_id, 1),
                        20,
                        block.height,
                        block.hash,
                    ),
                ],
                Vec::new(),
            )
        },
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let request = lightwalletd::GetAddressUtxosArg {
        addresses: vec![address_a, address_b.clone()],
        start_height: 1,
        max_entries: 2,
    };
    let list_response = adapter
        .get_address_utxos(Request::new(request.clone()))
        .await?
        .into_inner();
    let stream_response = adapter
        .get_address_utxos_stream(Request::new(request))
        .await?
        .into_inner();
    let streamed_utxos = collect_stream(stream_response).await?;
    let returned_txids: Vec<_> = streamed_utxos
        .iter()
        .map(|utxo| utxo.txid.clone())
        .collect();

    assert_eq!(list_response.address_utxos, streamed_utxos);
    assert_eq!(streamed_utxos.len(), 2);
    assert_eq!(
        returned_txids,
        vec![
            first_transaction_id.as_bytes().to_vec(),
            second_transaction_id.as_bytes().to_vec(),
        ]
    );
    assert!(streamed_utxos.iter().all(|utxo| utxo.address == address_b));

    Ok(())
}

#[tokio::test]
async fn send_transaction_forwards_accepted_to_zero_error_code() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let transaction_id = TransactionId::from_bytes([0x42; 32]);
    let broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let adapter = LightwalletdGrpcAdapter::new(WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster,
    ));

    let response = adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0xde, 0xad, 0xbe, 0xef],
            height: 0,
        }))
        .await?
        .into_inner();

    assert_eq!(response.error_code, 0);
    let mut expected_id = transaction_id.as_bytes();
    expected_id.reverse();
    assert_eq!(response.error_message, hex::encode(expected_id));

    Ok(())
}

#[tokio::test]
async fn send_transaction_maps_invalid_encoding_to_minus_22() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let broadcaster = MockTransactionBroadcaster::returning(
        TransactionBroadcastResult::InvalidEncoding(BroadcastInvalidEncoding {
            error_code: None,
            message: "TX decode failed".to_owned(),
        }),
    );
    let adapter = LightwalletdGrpcAdapter::new(WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster,
    ));

    let response = adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0xff],
            height: 0,
        }))
        .await?
        .into_inner();

    assert_eq!(response.error_code, -22);
    assert_eq!(response.error_message, "TX decode failed");

    Ok(())
}

#[tokio::test]
async fn send_transaction_maps_duplicate_and_rejected_codes() -> eyre::Result<()> {
    let cases = [
        (
            TransactionBroadcastResult::Duplicate(BroadcastDuplicate {
                error_code: None,
                message: "transaction already in mempool".to_owned(),
            }),
            -27,
            "transaction already in mempool",
        ),
        (
            TransactionBroadcastResult::Rejected(BroadcastRejected {
                error_code: None,
                message: "bad-txns-invalid".to_owned(),
            }),
            -26,
            "bad-txns-invalid",
        ),
        (
            TransactionBroadcastResult::Unknown(BroadcastUnknown {
                error_code: None,
                message: "node returned unclassified".to_owned(),
            }),
            -1,
            "node returned unclassified",
        ),
    ];

    for (broadcast_result, expected_code, expected_message) in cases {
        let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
        let broadcaster = MockTransactionBroadcaster::returning(broadcast_result);
        let adapter = LightwalletdGrpcAdapter::new(WalletQuery::new(
            store_fixture.chain_store().clone(),
            broadcaster,
        ));

        let response = adapter
            .send_transaction(Request::new(lightwalletd::RawTransaction {
                data: vec![0x00],
                height: 0,
            }))
            .await?
            .into_inner();

        assert_eq!(response.error_code, expected_code);
        assert_eq!(response.error_message, expected_message);
    }

    Ok(())
}

#[tokio::test]
async fn send_transaction_forwards_node_error_code_when_present() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let broadcaster = MockTransactionBroadcaster::returning(TransactionBroadcastResult::Rejected(
        BroadcastRejected {
            error_code: Some(-25),
            message: "missing-input".to_owned(),
        },
    ));
    let adapter = LightwalletdGrpcAdapter::new(WalletQuery::new(
        store_fixture.chain_store().clone(),
        broadcaster,
    ));

    let response = adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0x00],
            height: 0,
        }))
        .await?
        .into_inner();

    assert_eq!(response.error_code, -25);
    assert_eq!(response.error_message, "missing-input");

    Ok(())
}

#[tokio::test]
async fn send_transaction_reports_broadcast_disabled() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .send_transaction(Request::new(lightwalletd::RawTransaction {
            data: vec![0x00],
            height: 0,
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected disabled error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::FailedPrecondition);

    Ok(())
}

#[tokio::test]
async fn generated_lightwalletd_client_streams_over_grpc_transport() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let server_addr = listener.local_addr()?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()))
            .into_server();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_handle = tokio::spawn(async move {
        Server::builder()
            .add_service(adapter)
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async move {
                let _ = shutdown_rx.await;
            })
            .await
    });

    {
        let mut client = CompactTxStreamerClient::connect(format!("http://{server_addr}")).await?;
        let latest_block = client
            .get_latest_block(lightwalletd::ChainSpec {})
            .await?
            .into_inner();
        let mut compact_blocks = client
            .get_block_range(lightwalletd::BlockRange {
                start: Some(lightwalletd::BlockId {
                    height: latest_block.height,
                    hash: Vec::new(),
                }),
                end: Some(lightwalletd::BlockId {
                    height: latest_block.height,
                    hash: Vec::new(),
                }),
                pool_types: Vec::new(),
            })
            .await?
            .into_inner();
        let tree_state = client
            .get_latest_tree_state(lightwalletd::Empty {})
            .await?
            .into_inner();

        let compact_block = compact_blocks
            .message()
            .await?
            .ok_or_else(|| eyre!("missing compact block from compatibility stream"))?;

        assert!(compact_blocks.message().await?.is_none());
        assert_eq!(latest_block.height, 1);
        assert_eq!(compact_block.height, latest_block.height);
        assert_eq!(tree_state.height, latest_block.height);
    }

    let _ = shutdown_tx.send(());
    server_handle.await??;
    Ok(())
}

#[tokio::test]
async fn tree_state_reports_missing_non_empty_final_state_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":{"size":1}},"orchard":{"commitments":{}}}"#
            .to_vec(),
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!(
                "expected malformed tree-state error, got {response:?}"
            ));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_rejects_zero_empty_block_id_without_reader_fallback() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 0,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => return Err(eyre!("expected block 0 error, got {response:?}")),
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);

    Ok(())
}

#[tokio::test]
async fn tree_state_treats_absent_pool_and_empty_commitments_as_empty() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"orchard":{"commitments":{}}}"#.to_vec(),
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let tree_state = adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();

    assert_eq!(tree_state.sapling_tree, "");
    assert_eq!(tree_state.orchard_tree, "");

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_wrong_pool_shape_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"sapling":[]}"#.to_vec(),
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected malformed pool error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_wrong_commitments_shape_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":1296694002,"sapling":{"commitments":[]}}"#.to_vec(),
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!(
                "expected malformed commitments error, got {response:?}"
            ));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_missing_time_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"sapling":{"commitments":{}},"orchard":{"commitments":{}}}"#
            .to_vec(),
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected missing-time error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn tree_state_reports_wrong_time_type_as_data_loss() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(
        br#"{"hash":"010101","height":1,"time":"1296694002","sapling":{"commitments":{}},"orchard":{"commitments":{}}}"#
            .to_vec(),
    )?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_tree_state(Request::new(lightwalletd::BlockId {
            height: 1,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected wrong-time-type error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::DataLoss);

    Ok(())
}

#[tokio::test]
async fn ping_returns_zero_entry_and_exit() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let response = adapter
        .ping(Request::new(lightwalletd::Duration { interval_us: 0 }))
        .await?
        .into_inner();

    assert_eq!(response.entry, 0);
    assert_eq!(response.exit, 0);

    Ok(())
}

#[tokio::test]
async fn get_transaction_by_block_index_returns_indexed_transaction() -> eyre::Result<()> {
    let acceptance_txid_bytes = [2u8; 32];
    let store_fixture =
        acceptance_store_fixture_with_transactions(DEFAULT_TREE_STATE_PAYLOAD.to_vec(), |block| {
            vec![TransactionArtifact::new(
                TransactionId::from_bytes(acceptance_txid_bytes),
                block.height,
                block.hash,
                b"acceptance-transaction-bytes".to_vec(),
            )]
        })?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let response = adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            index: 0,
            hash: Vec::new(),
        }))
        .await?
        .into_inner();

    assert_eq!(response.height, 1);
    assert_eq!(response.data, b"acceptance-transaction-bytes");

    Ok(())
}

#[tokio::test]
async fn get_transaction_by_block_index_returns_not_found_for_unknown_index() -> eyre::Result<()> {
    let store_fixture = acceptance_store_fixture(DEFAULT_TREE_STATE_PAYLOAD.to_vec())?;
    let adapter =
        LightwalletdGrpcAdapter::new(WalletQuery::new(store_fixture.chain_store().clone(), ()));

    let status = match adapter
        .get_transaction(Request::new(lightwalletd::TxFilter {
            block: Some(lightwalletd::BlockId {
                height: 1,
                hash: Vec::new(),
            }),
            index: 99,
            hash: Vec::new(),
        }))
        .await
    {
        Ok(response) => {
            return Err(eyre!("expected not-found error, got {response:?}"));
        }
        Err(status) => status,
    };

    assert_eq!(status.code(), Code::NotFound);

    Ok(())
}

async fn collect_stream<T, Stream>(mut stream: Stream) -> Result<Vec<T>, tonic::Status>
where
    Stream: tonic::codegen::tokio_stream::Stream<Item = Result<T, tonic::Status>> + Unpin,
{
    use tonic::codegen::tokio_stream::StreamExt;

    let mut values = Vec::new();
    while let Some(stream_item) = stream.next().await {
        values.push(stream_item?);
    }
    Ok(values)
}

fn acceptance_store_fixture(tree_state_payload: Vec<u8>) -> eyre::Result<StoreFixture> {
    acceptance_store_fixture_with_transactions(tree_state_payload, |_| Vec::new())
}

fn acceptance_store_fixture_with_transactions<TransactionsFn>(
    tree_state_payload: Vec<u8>,
    build_transactions: TransactionsFn,
) -> eyre::Result<StoreFixture>
where
    TransactionsFn: FnOnce(&zinder_testkit::FixtureBlock) -> Vec<TransactionArtifact>,
{
    acceptance_store_fixture_with_transactions_and_transparent(
        tree_state_payload,
        build_transactions,
        |_| (Vec::new(), Vec::new()),
    )
}

fn acceptance_store_fixture_with_transactions_and_transparent<TransactionsFn, TransparentFn>(
    tree_state_payload: Vec<u8>,
    build_transactions: TransactionsFn,
    build_transparent_artifacts: TransparentFn,
) -> eyre::Result<StoreFixture>
where
    TransactionsFn: FnOnce(&zinder_testkit::FixtureBlock) -> Vec<TransactionArtifact>,
    TransparentFn: FnOnce(
        &zinder_testkit::FixtureBlock,
    ) -> (
        Vec<TransparentAddressUtxoArtifact>,
        Vec<TransparentUtxoSpendArtifact>,
    ),
{
    let base_fixture = ChainFixture::new(Network::ZcashRegtest)
        .extend_blocks(1)
        .with_tip_metadata_override(ChainTipMetadata::new(SUBTREE_LEAF_COUNT, 0))
        .with_tree_state_payload_at(ACCEPTANCE_BLOCK_HEIGHT, tree_state_payload);
    let acceptance_block = base_fixture
        .block_at(ACCEPTANCE_BLOCK_HEIGHT)
        .ok_or_else(|| eyre!("acceptance fixture must include the height 1 block"))?
        .clone();
    let block_hash = acceptance_block.hash;
    let parent_hash = acceptance_block.parent_hash;
    let block_time_seconds = acceptance_block.block_time_seconds;
    let transaction_artifacts = build_transactions(&acceptance_block);
    let (transparent_address_utxos, transparent_utxo_spends) =
        build_transparent_artifacts(&acceptance_block);

    let mut chain_fixture = base_fixture
        .with_compact_block_payload_at(
            ACCEPTANCE_BLOCK_HEIGHT,
            acceptance_compact_block_payload(block_hash, parent_hash, block_time_seconds),
        )
        .with_sapling_subtree_root(SubtreeRootArtifact::new(
            ShieldedProtocol::Sapling,
            SubtreeRootIndex::new(0),
            SubtreeRootHash::from_bytes(SAPLING_SUBTREE_ROOT_HASH),
            ACCEPTANCE_BLOCK_HEIGHT,
            block_hash,
        ));
    for transaction_artifact in transaction_artifacts {
        chain_fixture = chain_fixture.with_transaction_artifact(transaction_artifact);
    }
    for transparent_address_utxo in transparent_address_utxos {
        chain_fixture = chain_fixture.with_transparent_address_utxo(transparent_address_utxo);
    }
    for transparent_utxo_spend in transparent_utxo_spends {
        chain_fixture = chain_fixture.with_transparent_utxo_spend(transparent_utxo_spend);
    }

    Ok(StoreFixture::with_chain_committed(
        &chain_fixture,
        ChainEpochId::new(1),
    )?)
}

fn acceptance_compact_block_payload(
    block_hash: BlockHash,
    parent_hash: BlockHash,
    block_time_seconds: u32,
) -> Vec<u8> {
    lightwalletd::CompactBlock {
        proto_version: 1,
        height: 1,
        hash: block_hash.as_bytes().to_vec(),
        prev_hash: parent_hash.as_bytes().to_vec(),
        time: block_time_seconds,
        header: Vec::new(),
        vtx: vec![lightwalletd::CompactTx {
            index: 0,
            txid: vec![2; 32],
            fee: 0,
            spends: vec![lightwalletd::CompactSaplingSpend { nf: vec![3; 32] }],
            outputs: vec![lightwalletd::CompactSaplingOutput {
                cmu: vec![4; 32],
                ephemeral_key: vec![5; 32],
                ciphertext: vec![6; 52],
            }],
            actions: Vec::new(),
            vin: vec![lightwalletd::CompactTxIn {
                prevout_txid: vec![8; 32],
                prevout_index: 1,
            }],
            vout: vec![lightwalletd::TxOut {
                value: 5,
                script_pub_key: vec![0x51],
            }],
        }],
        chain_metadata: Some(lightwalletd::ChainMetadata {
            sapling_commitment_tree_size: SUBTREE_LEAF_COUNT,
            orchard_commitment_tree_size: 0,
        }),
    }
    .encode_to_vec()
}

fn transparent_address_script_hash(script_pub_key: &[u8]) -> TransparentAddressScriptHash {
    let mut hasher = Sha256::new();
    hasher.update(script_pub_key);
    TransparentAddressScriptHash::from_bytes(hasher.finalize().into())
}
