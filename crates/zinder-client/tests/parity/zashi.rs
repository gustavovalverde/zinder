//! Zashi / Zodl parity assertions.
//!
//! Zashi (mobile, via `zcash-android-wallet-sdk`) consumes the lightwalletd
//! compat surface and the federated `derive.explorer.*` surface. These
//! compile-time assertions ensure the `ChainIndex` methods Zashi's typed
//! gRPC paths depend on are present on the trait surface.
//!
//! Sourced from [closing-the-zaino-surface-gap.md](../../../../docs/reference/closing-the-zaino-surface-gap.md)
//! and [Android wallet integration findings](../../../../docs/reference/android-wallet-integration-findings.md).

use zinder_client::{ChainIndex, LocalChainIndex, RemoteChainIndex};

#[test]
fn parity_chain_index_surface_compiles_for_zashi_use_cases() {
    fn assert_compiles<T: ChainIndex>() {
        // closes G2: typed BlockSelector resolver (compact-block hash-only paths)
        let _ = T::block_id_by_selector;
        // closes G6: mempool point lookups for unmined UTXO overlays
        let _ = T::transparent_mempool_outputs_by_address;
        let _ = T::transparent_mempool_spend_by_outpoint;
        // closes G1: typed TransparentAddressBalance via federated derive
        let _ = T::transparent_address_balance;
        // closes G16, G21: typed subtree-root reads for SDK scan
        let _ = T::subtree_roots_in_range;
        // closes G3: typed TxStatus envelope (raw decode + status disambiguation)
        let _ = T::transaction_by_id;
    }
    assert_compiles::<LocalChainIndex>();
    assert_compiles::<RemoteChainIndex>();
}
