#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use zinder_client::{
    BlockHeight, ChainEpochId, ChainIndex, LocalChainIndex, LocalOpenOptions, Network,
    RemoteChainIndex, RemoteOpenOptions, TransactionArtifact, TransactionId, TransparentOutPoint,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{
    ChainFixture, P2pkhSpendArgs, StoreFixture, TransparentAddress as TestkitTransparentAddress,
    TransparentTestKey,
};

const FIXTURE_SEED: [u8; 32] = [
    0xAA, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x10,
    0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
];

#[tokio::test]
async fn local_and_remote_resolve_identical_transparent_prevouts() -> eyre::Result<()> {
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
    let wallet_query = WalletQuery::new(store_fixture.chain_store().clone(), ());
    let grpc_adapter = WalletQueryGrpcAdapter::new(wallet_query, ServerInfoSettings::default());
    let endpoint = spawn_wallet_query(grpc_adapter).await?;
    let remote = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })
    .await?;
    let local = LocalChainIndex::open(LocalOpenOptions {
        storage_path: store_fixture.tempdir_path().to_path_buf(),
        secondary_path: store_fixture.tempdir_path().join("zinder-client-secondary"),
        network: Network::ZcashRegtest,
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
    })
    .await?;

    let outpoints = vec![
        TransparentOutPoint::new(transaction_id, 0),
        TransparentOutPoint::new(TransactionId::from_bytes([0xEE; 32]), 0),
    ];
    let local_response = local.transparent_prevouts(&outpoints, None).await?;
    let remote_response = remote.transparent_prevouts(&outpoints, None).await?;

    assert_eq!(local_response.chain_epoch, remote_response.chain_epoch);
    assert_eq!(local_response.entries, remote_response.entries);
    assert_eq!(local_response.entries.len(), 2);
    assert!(local_response.entries[0].prevout.is_some());
    assert!(local_response.entries[1].prevout.is_none());
    Ok(())
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
