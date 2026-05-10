//! Zallet parity assertions.
//!
//! Zallet (the desktop wallet) is the primary `zinder-client::ChainIndex` Rust
//! consumer. These compile-time assertions ensure the `ChainIndex` methods
//! Zallet depends on stay present on the trait surface.
//! Renaming or removing any referenced method makes this module fail to
//! compile, gating Zallet's migration confidence at build time.

use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};

#[test]
fn parity_chain_index_surface_compiles_for_zallet_migration() {
    fn assert_compiles<T: ChainIndex>() {
        // typed BlockId from latest_block
        let _ = T::latest_block;
        // typed BlockSelector resolver
        let _ = T::block_id_by_selector;
        // typed BlockHeaderInfo
        let _ = T::block_header_by_selector;
        // typed TxStatus envelope (mined / mempool / conflicting)
        let _ = T::transaction_by_id;
        // standalone is_in_mempool boolean check
        let _ = T::is_in_mempool;
        // tree_state_at with Option<ChainEpoch>
        let _ = T::tree_state_at;
        // typed SubtreeRootHash + ShieldedProtocol enum
        let _ = T::subtree_roots_in_range;
        // typed RawTransactionBytes
        let _ = T::broadcast_transaction;
        // TipAdvanced as a typed signal in chain_events
        let _ = T::chain_events;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}
