#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{sync::Arc, time::Duration};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::StreamExt as _;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use zinder_client::{
    BlockHeight, ChainEpochId, ChainIndex, DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex,
    LocalOpenOptions, Network, RemoteChainIndex, RemoteOpenOptions, TransactionId,
    TransparentAddressScriptHash, TransparentAddressUnspentOutputsQuery,
    TransparentAddressUnspentOutputsStream, TransparentOutPoint, TransparentUnspentOutput,
    TransparentUnspentOutputStreamItem,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{ChainFixture, StoreFixture, sample_regtest_upgrade_activations};

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xCD; 32];
const SCRIPT_PUB_KEY: &[u8] = &[
    0x76, 0xa9, 0x14, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88,
    0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac,
];

async fn drain(
    stream: TransparentAddressUnspentOutputsStream,
) -> eyre::Result<Vec<TransparentUnspentOutputStreamItem>> {
    let mut items = Vec::new();
    let mut stream = stream;
    while let Some(stream_item) = stream.next().await {
        items.push(stream_item?);
    }
    Ok(items)
}

#[tokio::test]
async fn local_and_remote_streams_return_identical_unspent_sets() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(3).await?;
    let query = TransparentAddressUnspentOutputsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
    };
    let local_items = drain(
        fixtures
            .local
            .transparent_address_unspent_outputs(query.clone())
            .await?,
    )
    .await?;
    let remote_items = drain(
        fixtures
            .remote
            .transparent_address_unspent_outputs(query)
            .await?,
    )
    .await?;

    assert_eq!(local_items.len(), 3);
    assert_eq!(local_items, remote_items);
    let first_epoch = local_items
        .first()
        .map(|stream_item| stream_item.chain_epoch)
        .ok_or_else(|| eyre!("stream must not be empty"))?;
    assert!(
        local_items
            .iter()
            .all(|stream_item| stream_item.chain_epoch == first_epoch),
        "every item binds to the same pinned chain epoch",
    );
    Ok(())
}

#[tokio::test]
async fn local_and_remote_streams_honor_start_height_floor() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(3).await?;
    let query = TransparentAddressUnspentOutputsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(2),
    };
    let local_items = drain(
        fixtures
            .local
            .transparent_address_unspent_outputs(query.clone())
            .await?,
    )
    .await?;
    let remote_items = drain(
        fixtures
            .remote
            .transparent_address_unspent_outputs(query)
            .await?,
    )
    .await?;

    assert!(
        local_items.is_empty(),
        "outputs mined below the wallet-birthday floor are excluded",
    );
    assert_eq!(local_items, remote_items);
    Ok(())
}

struct ChainIndexFixtures {
    local: LocalChainIndex,
    remote: RemoteChainIndex,
    address_script_hash: TransparentAddressScriptHash,
    // Keeps the tempdir and primary store handle alive for the test's
    // lifetime; dropping it removes the data files the secondary store
    // and the in-process query are reading from.
    _store_fixture: StoreFixture,
}

async fn setup_chain_indexes(utxo_count: u32) -> eyre::Result<ChainIndexFixtures> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let mut chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (block_height, block_hash) = {
        let block = chain_fixture
            .block_at(BlockHeight::new(1))
            .ok_or_else(|| eyre!("fixture must contain block 1"))?;
        (block.height, block.hash)
    };
    for output_index in 0..utxo_count {
        let mut transaction_id_bytes = [0; 32];
        transaction_id_bytes[..4].copy_from_slice(&output_index.to_be_bytes());
        chain_fixture = chain_fixture.with_address_output_index(TransparentUnspentOutput::new(
            address_script_hash,
            SCRIPT_PUB_KEY.to_vec(),
            TransparentOutPoint::new(
                TransactionId::from_bytes(transaction_id_bytes),
                output_index,
            ),
            1_000_000_u64 + u64::from(output_index),
            block_height,
            block_hash,
        ));
    }
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
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        derive_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        initial_catchup_timeout: DEFAULT_INITIAL_CATCHUP_TIMEOUT,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        utxo_set_commitment_enabled: false,
    })
    .await?;
    Ok(ChainIndexFixtures {
        local,
        remote,
        address_script_hash,
        _store_fixture: store_fixture,
    })
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
