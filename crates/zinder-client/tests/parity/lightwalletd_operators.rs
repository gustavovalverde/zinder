//! Public lightwalletd operator parity assertions.
//!
//! `zec.rocks`-style operators expose the lightwalletd compat surface to
//! third-party clients. These compile-time assertions ensure the typed
//! `ChainIndex` methods that back compat surfaces remain on the trait.
//!
//! Cross-references: [Serving public lightwalletd clients](../../../../docs/reference/serving-public-lightwalletd-clients.md).

use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};

#[test]
fn parity_chain_index_surface_compiles_for_lightwalletd_operators() {
    fn assert_compiles<T: ChainIndex>() {
        // GetTaddressBalance backed by typed TransparentAddressBalance
        let _ = T::transparent_address_balance;
        // GetBlock and GetTreeState hash-only paths via BlockSelector
        let _ = T::block_id_by_selector;
        // GetSubtreeRoots typed bytes + typed pool enum
        let _ = T::subtree_roots_in_range;
        // SendTransaction typed bytes path
        let _ = T::broadcast_transaction;
        // TipAdvanced typed signal for the live chain-event stream
        let _ = T::chain_events;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}
