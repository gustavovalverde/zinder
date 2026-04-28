#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::time::Duration;

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tonic::transport::Server;
use zinder_client::{
    BlockHeight, BlockHeightRange, ChainEvent, ChainIndex, Network, RawTransactionBytes,
    RemoteChainIndex, RemoteOpenOptions, TransactionBroadcastResult, TransactionId,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{ChainFixture, MockTransactionBroadcaster, StoreFixture};

#[tokio::test]
async fn remote_chain_index_uses_native_grpc_for_m2_methods() -> eyre::Result<()> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let transaction_id = TransactionId::from_bytes([0x66; 32]);
    let broadcaster = MockTransactionBroadcaster::accepted(transaction_id);
    let wallet_query = WalletQuery::new(store_fixture.chain_store().clone(), broadcaster);
    let grpc_adapter = WalletQueryGrpcAdapter::new(
        wallet_query,
        ServerInfoSettings {
            transaction_broadcast_enabled: true,
            ..ServerInfoSettings::default()
        },
    );
    let endpoint = spawn_wallet_query(grpc_adapter).await?;
    let chain_index = RemoteChainIndex::connect(RemoteOpenOptions {
        endpoint,
        network: Network::ZcashRegtest,
    })
    .await?;

    let server_info = chain_index.server_info().await?;
    let current_epoch = chain_index.current_epoch().await?;
    let compact_block = chain_index.compact_block_at(BlockHeight::new(1)).await?;
    let mut compact_blocks = chain_index
        .compact_blocks_in_range(BlockHeightRange::inclusive(
            BlockHeight::new(1),
            BlockHeight::new(2),
        ))
        .await?;
    let mut compact_block_count = 0;
    while let Some(compact_block_result) = compact_blocks.next().await {
        compact_block_result?;
        compact_block_count += 1;
    }
    let broadcast_result = chain_index
        .broadcast_transaction(RawTransactionBytes::new([0x01, 0x02]))
        .await?;
    let mut events = chain_index.chain_events(None).await?;
    let first_event = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await?
        .ok_or_else(|| eyre!("chain-events stream closed before first event"))??;

    assert_eq!(server_info.network, Network::ZcashRegtest.name());
    assert!(
        server_info
            .capabilities
            .iter()
            .any(|capability| capability == "wallet.broadcast.transaction_v1")
    );
    assert_eq!(current_epoch.tip_height, BlockHeight::new(2));
    assert_eq!(compact_block.height, BlockHeight::new(1));
    assert_eq!(compact_block_count, 2);
    assert_eq!(
        broadcast_result,
        TransactionBroadcastResult::Accepted(zinder_client::BroadcastAccepted { transaction_id })
    );
    assert!(matches!(
        first_event.event,
        ChainEvent::ChainCommitted { committed }
            if committed.block_range.start == BlockHeight::new(1)
    ));
    assert!(!first_event.cursor.as_bytes().is_empty());

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
