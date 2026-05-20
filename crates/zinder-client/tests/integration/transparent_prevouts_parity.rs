#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{sync::Arc, time::Duration};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use zinder_client::{
    BlockHeight, ChainEpochId, ChainIndex, LocalChainIndex, LocalOpenOptions,
    MAX_TRANSPARENT_PREVOUTS_PER_REQUEST, Network, RemoteChainIndex, RemoteOpenOptions,
    TransactionArtifact, TransactionId, TransparentOutPoint,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{
    ChainFixture, P2pkhSpendArgs, StoreFixture, TransparentAddress as TestkitTransparentAddress,
    TransparentTestKey, sample_regtest_upgrade_activations,
};

const FIXTURE_SEED: [u8; 32] = [
    0xAA, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x10,
    0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
];

struct TransparentPrevoutParityHarness {
    _store_fixture: StoreFixture,
    local: LocalChainIndex,
    remote: RemoteChainIndex,
    indexed_transaction_id: TransactionId,
}

async fn transparent_prevout_parity_harness() -> eyre::Result<TransparentPrevoutParityHarness> {
    let key = TransparentTestKey::from_seed(&FIXTURE_SEED)?;
    let recipient = TestkitTransparentAddress::PublicKeyHash([0x42; 20]);
    let raw_bytes = key.build_p2pkh_spend(&P2pkhSpendArgs {
        coinbase_txid_be: [0xAA; 32],
        coinbase_vout: 0,
        coinbase_value_zats: 10_000_000,
        recipient: &recipient,
        target_height: 1,
    })?;
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let transaction_id = TransactionId::from_bytes([0xCC; 32]);
    let transaction_artifact =
        TransactionArtifact::new(transaction_id, block.height, block.hash, raw_bytes);
    let chain_fixture = chain_fixture.with_transaction_artifact(transaction_artifact);
    let store_fixture = StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    );
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    let endpoint = spawn_wallet_query(grpc_adapter).await?;
    let remote = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })?;
    let local = LocalChainIndex::open(LocalOpenOptions {
        storage_path: store_fixture.tempdir_path().to_path_buf(),
        secondary_path: store_fixture.tempdir_path().join("zinder-client-secondary"),
        network: Network::ZcashRegtest,
        storage_tuning: zinder_store::StorageTuning::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
    })
    .await?;

    Ok(TransparentPrevoutParityHarness {
        _store_fixture: store_fixture,
        local,
        remote,
        indexed_transaction_id: transaction_id,
    })
}

#[tokio::test]
async fn local_and_remote_resolve_identical_transparent_prevouts() -> eyre::Result<()> {
    let harness = transparent_prevout_parity_harness().await?;
    let outpoints = vec![
        TransparentOutPoint::new(harness.indexed_transaction_id, 0),
        TransparentOutPoint::new(TransactionId::from_bytes([0xEE; 32]), 0),
    ];
    let local_response = harness.local.transparent_prevouts(&outpoints, None).await?;
    let remote_response = harness
        .remote
        .transparent_prevouts(&outpoints, None)
        .await?;

    assert_eq!(local_response.chain_epoch, remote_response.chain_epoch);
    assert_eq!(local_response.entries, remote_response.entries);
    assert_eq!(local_response.entries.len(), 2);
    assert!(local_response.entries[0].prevout.is_some());
    assert!(local_response.entries[1].prevout.is_none());
    Ok(())
}

#[tokio::test]
async fn local_and_remote_truncate_transparent_prevout_requests() -> eyre::Result<()> {
    let harness = transparent_prevout_parity_harness().await?;
    let outpoints = oversized_outpoints();

    let local_response = harness.local.transparent_prevouts(&outpoints, None).await?;
    let remote_response = harness
        .remote
        .transparent_prevouts(&outpoints, None)
        .await?;

    assert_eq!(local_response.chain_epoch, remote_response.chain_epoch);
    assert_eq!(local_response.entries, remote_response.entries);
    assert_eq!(
        local_response.entries.len(),
        MAX_TRANSPARENT_PREVOUTS_PER_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn local_and_remote_reject_coinbase_sentinel_transparent_prevouts() -> eyre::Result<()> {
    let harness = transparent_prevout_parity_harness().await?;
    let outpoints = [TransparentOutPoint::COINBASE_SENTINEL];

    let local_error = match harness.local.transparent_prevouts(&outpoints, None).await {
        Ok(response) => {
            return Err(eyre!(
                "expected local coinbase sentinel rejection, got {response:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(local_error.to_string().contains("coinbase sentinel"));

    let remote_error = match harness.remote.transparent_prevouts(&outpoints, None).await {
        Ok(response) => {
            return Err(eyre!(
                "expected remote coinbase sentinel rejection, got {response:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(remote_error.to_string().contains("coinbase sentinel"));
    Ok(())
}

fn oversized_outpoints() -> Vec<TransparentOutPoint> {
    (0..=MAX_TRANSPARENT_PREVOUTS_PER_REQUEST)
        .map(|request_index| {
            let mut transaction_id_bytes = [0x55; 32];
            let index_bytes = request_index.to_le_bytes();
            transaction_id_bytes[30] = index_bytes[0];
            transaction_id_bytes[31] = index_bytes[1];
            TransparentOutPoint::new(TransactionId::from_bytes(transaction_id_bytes), 0)
        })
        .collect()
}

async fn spawn_wallet_query<QueryApi>(
    grpc_adapter: WalletQueryGrpcAdapter<QueryApi>,
) -> eyre::Result<String>
where
    WalletQueryGrpcAdapter<QueryApi>: zinder_proto::v1::wallet::wallet_query_server::WalletQuery,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        let _server_result = Server::builder()
            .add_service(grpc_adapter.into_server())
            .serve_with_incoming(incoming)
            .await;
    });
    Ok(format!("http://{addr}"))
}
