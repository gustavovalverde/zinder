//! Public lightwalletd operator parity assertions.
//!
//! `zec.rocks`-style operators expose the lightwalletd compat surface to
//! third-party clients. These compile-time assertions ensure the typed
//! `ChainIndex` methods that back compat surfaces remain on the trait.
//!
//! Sourced from [closing-the-zaino-surface-gap.md](../../../../docs/reference/closing-the-zaino-surface-gap.md)
//! and [Serving public lightwalletd clients](../../../../docs/reference/serving-public-lightwalletd-clients.md).

use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};

#[test]
fn parity_chain_index_surface_compiles_for_lightwalletd_operators() {
    fn assert_compiles<T: ChainIndex>() {
        // closes G1: GetTaddressBalance backed by typed TransparentAddressBalance
        let _ = T::transparent_address_balance;
        // closes G2: GetBlock and GetTreeState hash-only paths via BlockSelector
        let _ = T::block_id_by_selector;
        // closes G16, G21: GetSubtreeRoots typed bytes + typed pool enum
        let _ = T::subtree_roots_in_range;
        // closes G19: SendTransaction typed bytes path
        let _ = T::broadcast_transaction;
        // closes G20: TipAdvanced typed signal for the live chain-event stream
        let _ = T::chain_events;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}
