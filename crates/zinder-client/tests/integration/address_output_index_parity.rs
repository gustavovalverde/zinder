#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use zinder_client::{
    AddressOutputCursor, AddressOutputIndexArtifact, AddressOutputIndexQuery, BlockHeight,
    ChainEpochId, ChainIndex, DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex, LocalOpenOptions,
    Network, RemoteChainIndex, RemoteOpenOptions, TransactionId, TransparentAddressScriptHash,
    TransparentOutPoint,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{ChainFixture, StoreFixture, sample_regtest_upgrade_activations};

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xCD; 32];
const SCRIPT_PUB_KEY: &[u8] = &[
    0x76, 0xa9, 0x14, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88,
    0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac, 0x88, 0xac,
];

#[tokio::test]
async fn local_and_remote_drained_scan_returns_identical_utxos() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(3).await?;
    let drain_query = AddressOutputIndexQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        max_entries: None,
        from_cursor: None,
    };
    let local_view = fixtures
        .local
        .address_output_index(drain_query.clone(), None)
        .await?;
    let remote_view = fixtures
        .remote
        .address_output_index(drain_query, None)
        .await?;

    assert_eq!(local_view.outputs.len(), 3);
    assert_eq!(local_view.outputs, remote_view.outputs);
    assert_eq!(local_view.chain_epoch, remote_view.chain_epoch);
    assert_eq!(
        local_view
            .next_cursor
            .as_ref()
            .map(AddressOutputCursor::as_bytes),
        remote_view
            .next_cursor
            .as_ref()
            .map(AddressOutputCursor::as_bytes),
        "drained scan must surface identical cursor bytes across local and remote",
    );
    Ok(())
}

#[tokio::test]
async fn local_and_remote_paged_resume_returns_identical_utxos() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(3).await?;
    let first_page_query = AddressOutputIndexQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        max_entries: NonZeroU32::new(2),
        from_cursor: None,
    };
    let local_first = fixtures
        .local
        .address_output_index(first_page_query.clone(), None)
        .await?;
    let remote_first = fixtures
        .remote
        .address_output_index(first_page_query, None)
        .await?;

    assert_eq!(local_first.outputs.len(), 2);
    assert_eq!(local_first.outputs, remote_first.outputs);
    assert_eq!(
        local_first
            .next_cursor
            .as_ref()
            .map(AddressOutputCursor::as_bytes),
        remote_first
            .next_cursor
            .as_ref()
            .map(AddressOutputCursor::as_bytes),
        "paged cursor bytes must match between local and remote so resume is interoperable",
    );

    let local_cursor = local_first
        .next_cursor
        .clone()
        .ok_or_else(|| eyre!("local first page must yield a resume cursor"))?;
    let resume_query = AddressOutputIndexQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        max_entries: NonZeroU32::new(10),
        from_cursor: Some(local_cursor),
    };
    let local_resume = fixtures
        .local
        .address_output_index(resume_query.clone(), None)
        .await?;
    let remote_resume = fixtures
        .remote
        .address_output_index(resume_query, None)
        .await?;

    assert_eq!(local_resume.outputs, remote_resume.outputs);
    assert_eq!(
        local_first.outputs.len() + local_resume.outputs.len(),
        3,
        "first page plus resume must equal the full drained set",
    );
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
        chain_fixture = chain_fixture.with_address_output_index(AddressOutputIndexArtifact::new(
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
