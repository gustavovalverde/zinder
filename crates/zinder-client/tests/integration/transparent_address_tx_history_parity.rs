#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, time::Duration};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tonic::transport::Server;
use zinder_client::{
    BlockHeight, ChainEpochId, ChainIndex, IndexerError, LocalChainIndex, LocalOpenOptions,
    Network, RemoteChainIndex, RemoteOpenOptions, TransactionId, TransparentAddressScriptHash,
    TransparentAddressTxIdsQuery, TransparentAddressTxIdsStream, TransparentAddressTxIdsStreamItem,
    TransparentHistoryCursor,
};
use zinder_core::TransparentAddressTxIndexArtifact;
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_testkit::{ChainFixture, StoreFixture};

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xEF; 32];

#[tokio::test]
async fn local_and_remote_ascending_drain_returns_identical_tx_history() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(4).await?;
    let drain_query = TransparentAddressTxIdsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: None,
        from_cursor: None,
        descending: false,
    };
    let local_chunks = drain_stream(
        fixtures
            .local
            .transparent_address_tx_ids_in_range(drain_query.clone(), None)
            .await?,
    )
    .await?;
    let remote_chunks = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(drain_query, None)
            .await?,
    )
    .await?;

    assert_eq!(local_chunks.len(), 4);
    assert_chunk_lists_equal(&local_chunks, &remote_chunks);
    let tx_indexes: Vec<u32> = local_chunks
        .iter()
        .map(|chunk| chunk.artifact.tx_index_in_block)
        .collect();
    assert_eq!(tx_indexes, vec![0, 1, 2, 3]);
    Ok(())
}

#[tokio::test]
async fn local_and_remote_descending_drain_returns_identical_tx_history() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(4).await?;
    let descending_query = TransparentAddressTxIdsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: None,
        from_cursor: None,
        descending: true,
    };
    let local_chunks = drain_stream(
        fixtures
            .local
            .transparent_address_tx_ids_in_range(descending_query.clone(), None)
            .await?,
    )
    .await?;
    let remote_chunks = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(descending_query, None)
            .await?,
    )
    .await?;

    assert_chunk_lists_equal(&local_chunks, &remote_chunks);
    let tx_indexes: Vec<u32> = local_chunks
        .iter()
        .map(|chunk| chunk.artifact.tx_index_in_block)
        .collect();
    assert_eq!(tx_indexes, vec![3, 2, 1, 0]);
    Ok(())
}

#[tokio::test]
async fn local_and_remote_paged_resume_returns_identical_tx_history() -> eyre::Result<()> {
    let fixtures = setup_chain_indexes(4).await?;
    let first_page_query = TransparentAddressTxIdsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: NonZeroU32::new(2),
        from_cursor: None,
        descending: false,
    };
    let local_first = drain_stream(
        fixtures
            .local
            .transparent_address_tx_ids_in_range(first_page_query.clone(), None)
            .await?,
    )
    .await?;
    let remote_first = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(first_page_query, None)
            .await?,
    )
    .await?;
    assert_chunk_lists_equal(&local_first, &remote_first);
    assert_eq!(local_first.len(), 2);

    let local_cursor_bytes = last_cursor_bytes(&local_first)
        .ok_or_else(|| eyre!("local first page must yield a resume cursor"))?;
    let remote_cursor_bytes = last_cursor_bytes(&remote_first)
        .ok_or_else(|| eyre!("remote first page must yield a resume cursor"))?;
    assert_eq!(
        local_cursor_bytes, remote_cursor_bytes,
        "paged tx-history cursor bytes must match across local and remote",
    );

    let resume_query = TransparentAddressTxIdsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: NonZeroU32::new(10),
        from_cursor: Some(TransparentHistoryCursor::from_bytes(local_cursor_bytes)),
        descending: false,
    };
    let local_resume = drain_stream(
        fixtures
            .local
            .transparent_address_tx_ids_in_range(resume_query.clone(), None)
            .await?,
    )
    .await?;
    let remote_resume = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(resume_query, None)
            .await?,
    )
    .await?;
    assert_chunk_lists_equal(&local_resume, &remote_resume);
    assert_eq!(
        local_first.len() + local_resume.len(),
        4,
        "first page plus resumed page must equal the full drain",
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

async fn setup_chain_indexes(tx_count: u32) -> eyre::Result<ChainIndexFixtures> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let mut chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let (block_height, block_hash) = {
        let block = chain_fixture
            .block_at(BlockHeight::new(1))
            .ok_or_else(|| eyre!("fixture must contain block 1"))?;
        (block.height, block.hash)
    };
    for tx_index in 0..tx_count {
        let mut transaction_id_bytes = [0; 32];
        transaction_id_bytes[..4].copy_from_slice(&tx_index.to_be_bytes());
        chain_fixture = chain_fixture.with_transparent_address_tx_index(
            TransparentAddressTxIndexArtifact::new(
                address_script_hash,
                block_height,
                tx_index,
                TransactionId::from_bytes(transaction_id_bytes),
                block_hash,
            ),
        );
    }
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
    Ok(ChainIndexFixtures {
        local,
        remote,
        address_script_hash,
        _store_fixture: store_fixture,
    })
}

async fn drain_stream(
    mut stream: TransparentAddressTxIdsStream,
) -> Result<Vec<TransparentAddressTxIdsStreamItem>, IndexerError> {
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }
    Ok(chunks)
}

fn assert_chunk_lists_equal(
    left: &[TransparentAddressTxIdsStreamItem],
    right: &[TransparentAddressTxIdsStreamItem],
) {
    assert_eq!(left.len(), right.len());
    for (left_chunk, right_chunk) in left.iter().zip(right.iter()) {
        assert_eq!(left_chunk.artifact, right_chunk.artifact);
        assert_eq!(left_chunk.chain_epoch, right_chunk.chain_epoch);
        assert_eq!(
            left_chunk
                .cursor
                .as_ref()
                .map(TransparentHistoryCursor::as_bytes),
            right_chunk
                .cursor
                .as_ref()
                .map(TransparentHistoryCursor::as_bytes),
        );
    }
}

fn last_cursor_bytes(chunks: &[TransparentAddressTxIdsStreamItem]) -> Option<Vec<u8>> {
    chunks.last().and_then(|chunk| {
        chunk
            .cursor
            .as_ref()
            .map(|cursor| cursor.as_bytes().to_vec())
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
