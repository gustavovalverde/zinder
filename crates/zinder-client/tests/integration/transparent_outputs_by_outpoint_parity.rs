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
    BlockHeight, ChainEpochId, ChainIndex, DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex,
    LocalOpenOptions, MAX_TRANSPARENT_OUTPUTS_PER_REQUEST, Network, RemoteChainIndex,
    RemoteOpenOptions, TransactionId, TransparentAddressScriptHash, TransparentOutPoint,
    TransparentUnspentOutput,
};
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{ChainFixture, StoreFixture, sample_regtest_upgrade_activations};

struct TransparentOutputParityHarness {
    _store_fixture: StoreFixture,
    local: LocalChainIndex,
    remote: RemoteChainIndex,
    indexed_transaction_id: TransactionId,
}

async fn transparent_output_parity_harness() -> eyre::Result<TransparentOutputParityHarness> {
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let block_height = block.height;
    let block_hash = block.hash;
    let transaction_id = TransactionId::from_bytes([0xCC; 32]);
    let script_pub_key = vec![0x76, 0xa9, 0x42, 0x88, 0xac];
    let outpoint = TransparentOutPoint::new(transaction_id, 0);
    let chain_fixture = chain_fixture.with_address_output_index(TransparentUnspentOutput::new(
        TransparentAddressScriptHash::of_script_pub_key(&script_pub_key),
        script_pub_key,
        outpoint,
        10_000_000,
        block_height,
        block_hash,
    ));
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

    Ok(TransparentOutputParityHarness {
        _store_fixture: store_fixture,
        local,
        remote,
        indexed_transaction_id: transaction_id,
    })
}

#[tokio::test]
async fn local_and_remote_resolve_identical_transparent_outputs_by_outpoint() -> eyre::Result<()> {
    let harness = transparent_output_parity_harness().await?;
    let outpoints = vec![
        TransparentOutPoint::new(harness.indexed_transaction_id, 0),
        TransparentOutPoint::new(TransactionId::from_bytes([0xEE; 32]), 0),
    ];
    let local_response = harness
        .local
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await?;
    let remote_response = harness
        .remote
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await?;

    assert_eq!(local_response.chain_epoch, remote_response.chain_epoch);
    assert_eq!(local_response.entries, remote_response.entries);
    assert_eq!(local_response.entries.len(), 2);
    assert!(local_response.entries[0].output.is_some());
    assert!(local_response.entries[1].output.is_none());
    Ok(())
}

#[tokio::test]
async fn local_and_remote_truncate_transparent_output_requests() -> eyre::Result<()> {
    let harness = transparent_output_parity_harness().await?;
    let outpoints = oversized_outpoints();

    let local_response = harness
        .local
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await?;
    let remote_response = harness
        .remote
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await?;

    assert_eq!(local_response.chain_epoch, remote_response.chain_epoch);
    assert_eq!(local_response.entries, remote_response.entries);
    assert_eq!(
        local_response.entries.len(),
        MAX_TRANSPARENT_OUTPUTS_PER_REQUEST
    );
    Ok(())
}

#[tokio::test]
async fn local_and_remote_reject_coinbase_sentinel_transparent_outputs_by_outpoint()
-> eyre::Result<()> {
    let harness = transparent_output_parity_harness().await?;
    let outpoints = [TransparentOutPoint::COINBASE_SENTINEL];

    let local_error = match harness
        .local
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await
    {
        Ok(response) => {
            return Err(eyre!(
                "expected local coinbase sentinel rejection, got {response:?}"
            ));
        }
        Err(error) => error,
    };
    assert!(local_error.to_string().contains("coinbase sentinel"));

    let remote_error = match harness
        .remote
        .transparent_outputs_by_outpoint(&outpoints, None)
        .await
    {
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
    (0..=MAX_TRANSPARENT_OUTPUTS_PER_REQUEST)
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
