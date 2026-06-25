//! Zallet parity assertions.
//!
//! Zallet (the desktop wallet) is the primary `zinder-client` Rust consumer.
//! These compile-time assertions ensure the trait methods Zallet depends on
//! stay present. Base canonical reads live on `ChainIndex`; broadcast,
//! standalone mempool presence, and the chain-event stream need a live
//! endpoint, so they live on `EndpointBackedIndex` and a consumer that calls
//! them bounds its handle `T: ChainIndex + EndpointBackedIndex`. Renaming or
//! removing any referenced method makes this module fail to compile.

use eyre::eyre;
use zinder_client::{
    BlockHeight, BlockSelector, ChainIndex, EndpointBackedIndex, LocalChainIndex, RemoteChainIndex,
    SubtreeRootIndex, SubtreeRootRange, TransactionId, TxStatus,
};
use zinder_testkit::FixtureTransactionRows;

use super::{committed_store_fixture, open_local_chain_index, parity_chain_fixture};

#[test]
fn parity_chain_index_surface_compiles_for_zallet_native_contract() {
    fn assert_base_compiles<T: ChainIndex>() {
        // typed BlockId from latest_block
        let _ = T::latest_block;
        // typed BlockSelector resolver
        let _ = T::block_id_by_selector;
        // typed BlockHeaderInfo
        let _ = T::block_header_by_selector;
        // typed TxStatus envelope (mined / mempool / conflicting)
        let _ = T::transaction_by_id;
        // tree_state_at with Option<ChainEpoch>
        let _ = T::tree_state_at;
        // typed SubtreeRootHash + ShieldedProtocol enum
        let _ = T::subtree_roots_in_range;
    }
    fn assert_endpoint_compiles<T: EndpointBackedIndex>() {
        // standalone is_in_mempool boolean check
        let _ = T::is_in_mempool;
        // typed RawTransactionBytes
        let _ = T::broadcast_transaction;
        // ChainCommitted as a typed signal in chain_events
        let _ = T::chain_events;
    }
    assert_base_compiles::<LocalChainIndex>();
    assert_base_compiles::<RemoteChainIndex>();
    // Only the endpoint-backed adapter implements EndpointBackedIndex; a
    // LocalChainIndex handle is rejected here at compile time.
    assert_endpoint_compiles::<RemoteChainIndex>();
}

#[tokio::test]
async fn reads_epoch_bound_shape_from_fixture() -> eyre::Result<()> {
    let transaction_id = TransactionId::from_bytes([0x42; 32]);
    let base_fixture = parity_chain_fixture(2);
    let transaction_block = base_fixture
        .block_at(BlockHeight::new(2))
        .ok_or_else(|| eyre!("fixture must contain block 2"))?;
    let transaction_block_height = transaction_block.height;
    let transaction_block_hash = transaction_block.hash;
    let transaction_rows = FixtureTransactionRows::from_raw_transaction(
        transaction_id,
        transaction_block_height,
        transaction_block_hash,
        0,
        b"zallet-transaction-payload".to_vec(),
    );
    let transaction_location = transaction_rows.location;
    let chain_fixture = base_fixture.with_transaction_rows(transaction_rows);
    let store_fixture = committed_store_fixture(&chain_fixture)?;
    let chain_index = open_local_chain_index(&store_fixture).await?;

    let current_epoch = chain_index.current_epoch().await?;
    let pinned = Some(current_epoch.id);
    let latest_block = chain_index.latest_block(pinned).await?;
    let resolved_by_height = chain_index
        .block_id_by_selector(BlockSelector::Height(BlockHeight::new(2)), pinned)
        .await?;
    let resolved_by_hash = chain_index
        .block_id_by_selector(BlockSelector::Hash(transaction_block_hash), pinned)
        .await?;
    let tree_state = chain_index
        .tree_state_at(BlockHeight::new(2), pinned)
        .await?;
    let subtree_roots = chain_index
        .subtree_roots_in_range(
            SubtreeRootRange::new(
                zinder_client::ShieldedProtocol::Sapling,
                SubtreeRootIndex::new(0),
                std::num::NonZeroU32::MIN,
            ),
            pinned,
        )
        .await?;
    let mined_status = chain_index
        .transaction_by_id(transaction_id, pinned)
        .await?;
    let missing_status = chain_index
        .transaction_by_id(TransactionId::from_bytes([0x24; 32]), pinned)
        .await?;

    assert_eq!(latest_block, resolved_by_height);
    assert_eq!(latest_block, resolved_by_hash);
    assert_eq!(tree_state.height, BlockHeight::new(2));
    assert_eq!(tree_state.block_hash, transaction_block_hash);
    assert_eq!(subtree_roots.len(), 1);
    let TxStatus::Mined(mined) = mined_status else {
        return Err(eyre!("expected mined transaction, got {mined_status:?}"));
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
    assert_eq!(mined.details.confirmations, 1);
    assert_eq!(missing_status, TxStatus::NotFound);

    Ok(())
}
