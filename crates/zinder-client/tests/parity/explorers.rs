//! Block-explorer parity assertions.
//!
//! Block explorers exercise typed `WalletQuery` and federated
//! `derive.explorer.*` surfaces to display per-block, per-transaction, and
//! per-address state. These compile-time assertions ensure the trait surface
//! they depend on stays intact through future refactors.
//!
//! Sourced from [closing-the-zaino-surface-gap.md](../../../../docs/reference/closing-the-zaino-surface-gap.md).

use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};

#[test]
fn parity_chain_index_surface_compiles_for_block_explorers() {
    fn assert_compiles<T: ChainIndex>() {
        // closes G2: hash-or-height lookup via BlockSelector
        let _ = T::block_id_by_selector;
        // closes G4: typed block-header read model
        let _ = T::block_header_by_selector;
        // closes G3, G7, G13: typed TxStatus with mined / mempool / conflicting
        let _ = T::transaction_by_id;
        // closes G6: per-address mempool overlays
        let _ = T::transparent_mempool_outputs_by_address;
        let _ = T::transparent_mempool_spend_by_outpoint;
        // closes G1: typed TransparentAddressBalance via federated derive
        let _ = T::transparent_address_balance;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}
