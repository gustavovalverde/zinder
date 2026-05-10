//! Zallet parity assertions.
//!
//! Zallet (the desktop wallet) is the primary `zinder-client::ChainIndex` Rust
//! consumer. These compile-time assertions ensure the `ChainIndex` methods
//! Zallet's planned migration depends on are present on the trait surface.
//! Renaming or removing any referenced method makes this module fail to
//! compile, gating Zallet's migration confidence at build time.
//!
//! Sourced from [closing-the-zaino-surface-gap.md](../../../../docs/reference/closing-the-zaino-surface-gap.md);
//! each `let _ = T::name` line corresponds to a closed gap row whose `closes:`
//! tag points at the method.

use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};

#[test]
fn parity_chain_index_surface_compiles_for_zallet_migration() {
    fn assert_compiles<T: ChainIndex>() {
        // closes G15: typed BlockId from latest_block
        let _ = T::latest_block;
        // closes G2: typed BlockSelector resolver
        let _ = T::block_id_by_selector;
        // closes G4: typed BlockHeaderInfo
        let _ = T::block_header_by_selector;
        // closes G3, G7, G13: typed TxStatus envelope (mined / mempool / conflicting)
        let _ = T::transaction_by_id;
        // closes G7: standalone is_in_mempool boolean check
        let _ = T::is_in_mempool;
        // closes G17: tree_state_at with Option<ChainEpoch>
        let _ = T::tree_state_at;
        // closes G16, G21: typed SubtreeRootHash + ShieldedProtocol enum
        let _ = T::subtree_roots_in_range;
        // closes G19: typed RawTransactionBytes
        let _ = T::broadcast_transaction;
        // closes G20: TipAdvanced as a typed signal in chain_events
        let _ = T::chain_events;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}
