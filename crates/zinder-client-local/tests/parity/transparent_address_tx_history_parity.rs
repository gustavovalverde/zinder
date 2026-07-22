#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use eyre::eyre;
use tokio::net::TcpListener;
use tokio_stream::{StreamExt as _, wrappers::TcpListenerStream};
use tonic::transport::Server;
use zinder_client::{
    BlockHeight, ChainEpochId, ChainIndex, IndexerError, Network, RemoteChainIndex,
    RemoteOpenOptions, TransactionId, TransparentAddressScriptHash,
    TransparentAddressTransactionChunk, TransparentAddressTxIdsQuery,
    TransparentAddressTxIdsStream, TransparentHistoryCursor,
};
use zinder_client_local::{DEFAULT_INITIAL_CATCHUP_TIMEOUT, LocalChainIndex, LocalOpenOptions};
use zinder_core::TransparentAddressTxIndexArtifact;
use zinder_materialized_views::TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME;
use zinder_query::{ServerInfoSettings, WalletQuery, WalletQueryGrpcAdapter};
use zinder_store::{ChainEventStreamFamily, EventStreamStartPosition, ReorgWindowChange};
use zinder_testkit::{
    ChainFixture, StoreFixture, open_test_materialized_view_store_for_canonical,
    sample_regtest_upgrade_activations, seed_transparent_address_transaction_history,
};

const ADDRESS_SCRIPT_HASH_BYTES: [u8; 32] = [0xEF; 32];

#[tokio::test]
async fn local_and_remote_ascending_drain_return_identical_transaction_history() -> eyre::Result<()>
{
    let fixtures = setup_chain_indexes(4).await?;
    let query = TransparentAddressTxIdsQuery {
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
            .transparent_address_tx_ids_in_range(query.clone())
            .await?,
    )
    .await?;
    let remote_chunks = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(query)
            .await?,
    )
    .await?;

    assert_eq!(local_chunks.len(), 4);
    assert_chunk_lists_equal(&local_chunks, &remote_chunks);
    assert_eq!(transaction_indexes(&local_chunks), vec![0, 1, 2, 3]);
    Ok(())
}

#[tokio::test]
async fn local_and_remote_descending_drain_return_identical_transaction_history() -> eyre::Result<()>
{
    let fixtures = setup_chain_indexes(4).await?;
    let query = TransparentAddressTxIdsQuery {
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
            .transparent_address_tx_ids_in_range(query.clone())
            .await?,
    )
    .await?;
    let remote_chunks = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(query)
            .await?,
    )
    .await?;

    assert_chunk_lists_equal(&local_chunks, &remote_chunks);
    assert_eq!(transaction_indexes(&local_chunks), vec![3, 2, 1, 0]);
    Ok(())
}

#[tokio::test]
async fn local_and_remote_cursor_resume_return_identical_transaction_history() -> eyre::Result<()> {
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
            .transparent_address_tx_ids_in_range(first_page_query.clone())
            .await?,
    )
    .await?;
    let remote_first = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(first_page_query)
            .await?,
    )
    .await?;
    assert_chunk_lists_equal(&local_first, &remote_first);
    assert_eq!(local_first.len(), 2);

    let local_cursor_bytes = last_cursor_bytes(&local_first)
        .ok_or_else(|| eyre!("local first page must yield a resume cursor"))?;
    let remote_cursor_bytes = last_cursor_bytes(&remote_first)
        .ok_or_else(|| eyre!("remote first page must yield a resume cursor"))?;
    assert_eq!(local_cursor_bytes, remote_cursor_bytes);

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
            .transparent_address_tx_ids_in_range(resume_query.clone())
            .await?,
    )
    .await?;
    let remote_resume = drain_stream(
        fixtures
            .remote
            .transparent_address_tx_ids_in_range(resume_query)
            .await?,
    )
    .await?;

    assert_chunk_lists_equal(&local_resume, &remote_resume);
    assert_eq!(local_first.len() + local_resume.len(), 4);
    Ok(())
}

#[tokio::test]
async fn local_and_remote_refuse_history_when_the_visible_chain_fence_differs() -> eyre::Result<()>
{
    let fixtures = setup_chain_indexes(1).await?;
    fixtures.materialized_view_store.put_chain_event_cursor(
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        &[0xFF],
    )?;
    let query = TransparentAddressTxIdsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: None,
        from_cursor: None,
        descending: false,
    };

    let Err(local_error) = fixtures
        .local
        .transparent_address_tx_ids_in_range(query.clone())
        .await
    else {
        return Err(eyre!(
            "local history accepted another visible chain's materialized view"
        ));
    };
    let Err(remote_error) = fixtures
        .remote
        .transparent_address_tx_ids_in_range(query)
        .await
    else {
        return Err(eyre!(
            "remote history accepted another visible chain's materialized view"
        ));
    };
    assert!(matches!(
        local_error,
        IndexerError::FailedPrecondition { .. }
    ));
    assert!(matches!(
        remote_error,
        IndexerError::FailedPrecondition { .. }
    ));
    Ok(())
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "The scenario intentionally keeps canonical A to B, materialized-view, local, and gRPC assertions together."
)]
async fn local_and_remote_fence_history_across_a_same_height_reorg() -> eyre::Result<()> {
    let branch_a = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let branch_b = branch_a.fork_at(BlockHeight::new(2))?.extend_blocks(1);
    let mut initial_artifacts = branch_a
        .chain_epoch_artifacts(ChainEpochId::new(1))
        .ok_or_else(|| eyre!("branch A must contain a visible chain epoch"))?;
    initial_artifacts.chain_epoch.settled_tip_height = BlockHeight::new(0);
    initial_artifacts.chain_epoch.settled_tip_hash = Network::ZcashRegtest.genesis_hash();
    let store_fixture = StoreFixture::open()?;
    store_fixture
        .chain_store()
        .commit_chain_epoch(initial_artifacts)?;
    let materialized_view_store =
        open_test_materialized_view_store_for_canonical(store_fixture.tempdir_path())?;
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let initial_tip_block = branch_a
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("branch A must contain height 2"))?;
    let initial_history_artifact = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        initial_tip_block.height,
        0,
        TransactionId::from_bytes([0xA1; 32]),
        initial_tip_block.hash,
    );
    seed_transparent_address_transaction_history(
        &materialized_view_store,
        &[initial_history_artifact],
    )?;
    let initial_fence = visible_chain_fence(store_fixture.chain_store())?;
    materialized_view_store.put_chain_event_cursor(
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        &initial_fence,
    )?;

    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    )
    .with_materialized_view_store(materialized_view_store.clone());
    let grpc_adapter = WalletQueryGrpcAdapter::new(
        wallet_query,
        ServerInfoSettings {
            transparent_address_history_available: true,
            ..ServerInfoSettings::default()
        },
    );
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
        materialized_view_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        initial_catchup_timeout: DEFAULT_INITIAL_CATCHUP_TIMEOUT,
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
        utxo_set_commitment_enabled: false,
    })
    .await?;

    let first_page_query = TransparentAddressTxIdsQuery {
        address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: NonZeroU32::new(1),
        from_cursor: None,
        descending: false,
    };
    let mut first_page = local
        .transparent_address_tx_ids_in_range(first_page_query)
        .await?;
    let first_chunk = first_page
        .next()
        .await
        .ok_or_else(|| eyre!("first page must contain a history chunk"))??;
    let stale_cursor = first_chunk
        .cursor
        .ok_or_else(|| eyre!("branch A first page must expose a resume cursor"))?;

    let mut replacement_artifacts = branch_b
        .chain_epoch_artifacts(ChainEpochId::new(2))
        .ok_or_else(|| eyre!("branch B must contain a replacement epoch"))?;
    replacement_artifacts.chain_epoch.settled_tip_height = BlockHeight::new(0);
    replacement_artifacts.chain_epoch.settled_tip_hash = Network::ZcashRegtest.genesis_hash();
    replacement_artifacts
        .block_headers
        .retain(|header| header.height == BlockHeight::new(2));
    replacement_artifacts.block_replay_envelopes = branch_b
        .block_replay_envelopes()
        .into_iter()
        .skip(1)
        .collect();
    replacement_artifacts
        .compact_blocks
        .retain(|block| block.height() == BlockHeight::new(2));
    replacement_artifacts.reorg_window_change = ReorgWindowChange::Replace {
        from_height: BlockHeight::new(2),
    };
    let replacement_epoch = replacement_artifacts.chain_epoch;
    let replacement_tip_block = branch_b
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("branch B must contain height 2"))?;
    let replacement_history_artifact = TransparentAddressTxIndexArtifact::new(
        address_script_hash,
        replacement_tip_block.height,
        0,
        TransactionId::from_bytes([0xB2; 32]),
        replacement_tip_block.hash,
    );
    store_fixture
        .chain_store()
        .commit_chain_epoch(replacement_artifacts)?;

    let query = TransparentAddressTxIdsQuery {
        address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: None,
        from_cursor: None,
        descending: false,
    };
    let Err(local_error) = local
        .transparent_address_tx_ids_in_range(query.clone())
        .await
    else {
        return Err(eyre!(
            "local history accepted branch A after canonical branch B replaced it"
        ));
    };
    let Err(remote_error) = remote
        .transparent_address_tx_ids_in_range(query.clone())
        .await
    else {
        return Err(eyre!(
            "remote history accepted branch A after canonical branch B replaced it"
        ));
    };
    assert!(matches!(
        local_error,
        IndexerError::FailedPrecondition { .. }
    ));
    assert!(matches!(
        remote_error,
        IndexerError::FailedPrecondition { .. }
    ));

    seed_transparent_address_transaction_history(
        &materialized_view_store,
        &[replacement_history_artifact],
    )?;
    let replacement_fence = visible_chain_fence(store_fixture.chain_store())?;
    assert_ne!(initial_fence, replacement_fence);
    materialized_view_store.put_chain_event_cursor(
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        &replacement_fence,
    )?;

    let local_branch_b = drain_stream(
        local
            .transparent_address_tx_ids_in_range(query.clone())
            .await?,
    )
    .await?;
    let remote_branch_b =
        drain_stream(remote.transparent_address_tx_ids_in_range(query).await?).await?;
    assert_chunk_lists_equal(&local_branch_b, &remote_branch_b);
    assert_eq!(local_branch_b.len(), 1);
    assert_eq!(local_branch_b[0].chain_epoch, replacement_epoch);
    assert_eq!(local_branch_b[0].artifact, replacement_history_artifact);

    let stale_cursor_query = TransparentAddressTxIdsQuery {
        address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: NonZeroU32::new(1),
        from_cursor: Some(stale_cursor),
        descending: false,
    };
    let Err(local_error) = local
        .transparent_address_tx_ids_in_range(stale_cursor_query.clone())
        .await
    else {
        return Err(eyre!("local history resumed a cursor issued on branch A"));
    };
    let Err(remote_error) = remote
        .transparent_address_tx_ids_in_range(stale_cursor_query)
        .await
    else {
        return Err(eyre!("remote history resumed a cursor issued on branch A"));
    };
    assert!(matches!(
        local_error,
        IndexerError::FailedPrecondition { .. }
    ));
    assert!(matches!(
        remote_error,
        IndexerError::FailedPrecondition { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn local_and_remote_classify_invalid_history_cursors_as_invalid_requests() -> eyre::Result<()>
{
    let fixtures = setup_chain_indexes(2).await?;
    let first_page_query = TransparentAddressTxIdsQuery {
        address_script_hash: fixtures.address_script_hash,
        start_height: BlockHeight::new(0),
        end_height: BlockHeight::new(100),
        max_entries: NonZeroU32::new(1),
        from_cursor: None,
        descending: false,
    };
    let first_page = drain_stream(
        fixtures
            .local
            .transparent_address_tx_ids_in_range(first_page_query.clone())
            .await?,
    )
    .await?;
    let valid_cursor = last_cursor_bytes(&first_page)
        .ok_or_else(|| eyre!("first history page must expose a resume cursor"))?;

    let mut invalid_prefix = valid_cursor.clone();
    let prefix_byte = invalid_prefix
        .first_mut()
        .ok_or_else(|| eyre!("history cursor must carry a prefix"))?;
    *prefix_byte ^= 0xFF;
    let mut invalid_length = valid_cursor.clone();
    let _ = invalid_length
        .pop()
        .ok_or_else(|| eyre!("history cursor must carry a key"))?;
    let mut invalid_direction = valid_cursor.clone();
    let direction_byte = invalid_direction
        .get_mut(4)
        .ok_or_else(|| eyre!("history cursor must carry a direction"))?;
    *direction_byte = 2;

    let cursor_query = |cursor, address_script_hash, end_height| TransparentAddressTxIdsQuery {
        address_script_hash,
        start_height: BlockHeight::new(0),
        end_height,
        max_entries: NonZeroU32::new(1),
        from_cursor: Some(TransparentHistoryCursor::from_bytes(cursor)),
        descending: false,
    };
    let invalid_queries = [
        cursor_query(
            invalid_prefix,
            fixtures.address_script_hash,
            BlockHeight::new(100),
        ),
        cursor_query(
            invalid_length,
            fixtures.address_script_hash,
            BlockHeight::new(100),
        ),
        cursor_query(
            invalid_direction,
            fixtures.address_script_hash,
            BlockHeight::new(100),
        ),
        cursor_query(
            valid_cursor.clone(),
            TransparentAddressScriptHash::from_bytes([0xAA; 32]),
            BlockHeight::new(100),
        ),
        cursor_query(
            valid_cursor,
            fixtures.address_script_hash,
            BlockHeight::new(0),
        ),
    ];

    for query in invalid_queries {
        assert_invalid_history_cursor(&fixtures, query).await?;
    }
    Ok(())
}

async fn assert_invalid_history_cursor(
    fixtures: &ChainIndexFixtures,
    query: TransparentAddressTxIdsQuery,
) -> eyre::Result<()> {
    let Err(local_error) = fixtures
        .local
        .transparent_address_tx_ids_in_range(query.clone())
        .await
    else {
        return Err(eyre!("local history accepted a malformed cursor"));
    };
    let Err(remote_error) = fixtures
        .remote
        .transparent_address_tx_ids_in_range(query)
        .await
    else {
        return Err(eyre!("remote history accepted a malformed cursor"));
    };
    assert!(matches!(local_error, IndexerError::InvalidRequest { .. }));
    assert!(matches!(remote_error, IndexerError::InvalidRequest { .. }));
    Ok(())
}

struct ChainIndexFixtures {
    local: LocalChainIndex,
    remote: RemoteChainIndex,
    address_script_hash: TransparentAddressScriptHash,
    _store_fixture: StoreFixture,
    materialized_view_store: zinder_materialized_views::MaterializedViewStore,
}

async fn setup_chain_indexes(tx_count: u32) -> eyre::Result<ChainIndexFixtures> {
    let address_script_hash = TransparentAddressScriptHash::from_bytes(ADDRESS_SCRIPT_HASH_BYTES);
    let chain_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(1);
    let block = chain_fixture
        .block_at(BlockHeight::new(1))
        .ok_or_else(|| eyre!("fixture must contain block 1"))?;
    let artifacts = (0..tx_count)
        .map(|tx_index| {
            let mut transaction_id_bytes = [0; 32];
            transaction_id_bytes[..4].copy_from_slice(&tx_index.to_be_bytes());
            TransparentAddressTxIndexArtifact::new(
                address_script_hash,
                block.height,
                tx_index,
                TransactionId::from_bytes(transaction_id_bytes),
                block.hash,
            )
        })
        .collect::<Vec<_>>();
    let store_fixture = StoreFixture::with_chain_committed(&chain_fixture, ChainEpochId::new(1))?;
    let materialized_view_store =
        open_test_materialized_view_store_for_canonical(store_fixture.tempdir_path())?;
    seed_transparent_address_transaction_history(&materialized_view_store, &artifacts)?;
    let chain_fence = store_fixture
        .chain_store()
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )?
        .cursor
        .ok_or_else(|| eyre!("committed fixture must have a visible chain fence"))?;
    materialized_view_store.put_chain_event_cursor(
        TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_CONSUMER_NAME,
        chain_fence.as_bytes(),
    )?;
    let wallet_query = WalletQuery::new(
        store_fixture.chain_store().clone(),
        (),
        Arc::new(sample_regtest_upgrade_activations()),
    )
    .with_materialized_view_store(materialized_view_store.clone());
    let grpc_adapter = WalletQueryGrpcAdapter::new(
        wallet_query,
        ServerInfoSettings {
            transparent_address_history_available: true,
            ..ServerInfoSettings::default()
        },
    );
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
        materialized_view_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
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
        materialized_view_store,
    })
}

fn visible_chain_fence(store: &zinder_store::PrimaryChainStore) -> eyre::Result<Vec<u8>> {
    store
        .resolve_chain_event_stream_start(
            &EventStreamStartPosition::LiveTail,
            ChainEventStreamFamily::Visible,
        )?
        .cursor
        .map(|cursor| cursor.as_bytes().to_vec())
        .ok_or_else(|| eyre!("committed fixture must have a visible chain fence"))
}

async fn drain_stream(
    mut stream: TransparentAddressTxIdsStream,
) -> Result<Vec<TransparentAddressTransactionChunk>, IndexerError> {
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(chunk?);
    }
    Ok(chunks)
}

fn assert_chunk_lists_equal(
    left: &[TransparentAddressTransactionChunk],
    right: &[TransparentAddressTransactionChunk],
) {
    assert_eq!(left, right);
}

fn transaction_indexes(chunks: &[TransparentAddressTransactionChunk]) -> Vec<u32> {
    chunks
        .iter()
        .map(|chunk| chunk.artifact.tx_index_in_block)
        .collect()
}

fn last_cursor_bytes(chunks: &[TransparentAddressTransactionChunk]) -> Option<Vec<u8>> {
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
    let address = listener.local_addr()?;
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        let _server_result = Server::builder()
            .add_service(grpc_adapter.into_server())
            .serve_with_incoming(incoming)
            .await;
    });
    Ok(format!("http://{address}"))
}
