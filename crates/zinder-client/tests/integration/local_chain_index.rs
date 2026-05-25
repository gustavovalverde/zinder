#![allow(
    missing_docs,
    reason = "Integration test names describe the behavior under test."
)]

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use eyre::eyre;
use tokio_stream::StreamExt as _;
use zinder_client::{
    BlockHeight, BlockHeightRange, ChainIndex, IndexerError, LocalChainIndex, LocalOpenOptions,
    Network, ShieldedProtocol, SubtreeRootIndex, SubtreeRootRange, TransactionId, TxStatus,
};
use zinder_testkit::{
    ChainFixture, FixtureTransactionRows, StoreFixture, sample_regtest_upgrade_activations,
};

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "single integration scenario keeps secondary-store reads on one pinned fixture"
)]
async fn local_chain_index_reads_typed_values_from_secondary_store() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x44; 32]);
    let base_fixture = ChainFixture::new(Network::ZcashRegtest).extend_blocks(2);
    let transaction_block = base_fixture
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("fixture must contain block 2"))?;
    let transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        transaction_block.height,
        transaction_block.hash,
        0,
        b"transaction-payload".to_vec(),
    );
    let transaction_location = transaction_rows.location;
    let chain_fixture = base_fixture.with_transaction_rows(transaction_rows);
    let store_fixture =
        StoreFixture::with_chain_committed(&chain_fixture, zinder_client::ChainEpochId::new(1))?;
    let chain_index = LocalChainIndex::open(LocalOpenOptions {
        storage_path: store_fixture.tempdir_path().to_path_buf(),
        secondary_path: store_fixture.tempdir_path().join("zinder-client-secondary"),
        network: Network::ZcashRegtest,
        canonical_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        derive_rocksdb_budget: zinder_store::RocksDbResourceBudget::for_local_tests(),
        subscription_endpoint: None,
        catchup_interval: Duration::from_millis(20),
        network_upgrade_activations: Arc::new(sample_regtest_upgrade_activations()),
    })
    .await?;

    let current_epoch = chain_index.current_epoch().await?;
    let latest_block = chain_index.latest_block(None).await?;
    let compact_block = chain_index
        .compact_block_at(BlockHeight::new(1), None)
        .await?;
    let tree_state = chain_index.latest_tree_state_checkpoint(None).await?;
    let subtree_roots = chain_index
        .subtree_roots_in_range(
            SubtreeRootRange::new(
                ShieldedProtocol::Sapling,
                SubtreeRootIndex::new(0),
                NonZeroU32::MIN,
            ),
            None,
        )
        .await?;
    let mined_transaction = chain_index.transaction_by_id(transaction_id, None).await?;
    let missing_transaction = chain_index
        .transaction_by_id(TransactionId::from_bytes([0x55; 32]), None)
        .await?;
    let mut compact_block_stream = chain_index
        .compact_blocks_in_range(
            BlockHeightRange::inclusive(BlockHeight::new(1), BlockHeight::new(2)),
            None,
        )
        .await?;
    let mut compact_block_count = 0;
    while let Some(compact_block_result) = compact_block_stream.next().await {
        compact_block_result?;
        compact_block_count += 1;
    }

    assert_eq!(current_epoch.tip_height, BlockHeight::new(2));
    assert_eq!(latest_block.height, BlockHeight::new(2));
    assert_eq!(compact_block.height, BlockHeight::new(1));
    assert_eq!(tree_state.height, BlockHeight::new(2));
    assert_eq!(subtree_roots.len(), 1);
    let TxStatus::Mined(mined) = mined_transaction else {
        return Err(eyre!(
            "expected mined transaction, got {mined_transaction:?}"
        ));
    };
    assert_eq!(
        mined.location.transaction_id,
        transaction_location.transaction_id
    );
    assert_eq!(
        mined.location.block_height,
        transaction_location.block_height
    );
    assert_eq!(mined.location.block_hash, transaction_location.block_hash);
    assert_eq!(mined.location.tx_index_in_block, 0);
    assert_eq!(missing_transaction, TxStatus::NotFound);
    assert_eq!(compact_block_count, 2);
    assert!(matches!(
        chain_index.chain_events(None).await,
        Err(IndexerError::RemoteEndpointUnconfigured {
            operation: "chain_events"
        })
    ));

    Ok(())
}
