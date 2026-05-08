//! Capability-coverage contract test.
//!
//! Every capability string advertised in [`ZINDER_CAPABILITIES`] must map to
//! a documented method (`ChainIndex` for `wallet.*`, `ExplorerQuery` for
//! `derive.*`). Adding a new capability without a method is a contract
//! violation: clients that gate on the string would see it advertised yet
//! have no way to call the underlying read.
//!
//! The mapping is maintained alongside this test. Adding a new capability
//! requires extending both `ZINDER_CAPABILITIES` and `EXPECTED_METHOD_NAMES`
//! in the same change. Adding a new method without a capability is a softer
//! warning (the test doesn't enforce that direction) because some methods
//! are private to development tooling and not part of the public capability
//! spine.
//!
//! `assert_wallet_chain_index_methods_compile` below takes a function-item
//! reference to each `ChainIndex` method named in `EXPECTED_METHOD_NAMES`.
//! Renaming or removing any of those methods on the trait makes this test
//! file fail to compile, so the table is bound to the trait at build time
//! instead of only at string-comparison time. Capabilities under the
//! `derive.*` namespace target `ExplorerQuery` server types and are checked
//! by the consuming derive crate; this test treats them as opaque table
//! entries.

use zinder_client::{ChainIndex, ZINDER_CAPABILITIES};

/// Capability-to-method coverage table. The `ChainIndex` trait surface is
/// reflected by name; the test walks `ZINDER_CAPABILITIES` and asserts every
/// entry has a corresponding row here.
const EXPECTED_METHOD_NAMES: &[(&str, &str)] = &[
    ("wallet.read.latest_block_v1", "latest_block"),
    ("wallet.read.compact_block_at_v1", "compact_block_at"),
    (
        "wallet.read.compact_block_range_v1",
        "compact_blocks_in_range",
    ),
    ("wallet.read.tree_state_at_v1", "tree_state_at"),
    ("wallet.read.latest_tree_state_v1", "latest_tree_state"),
    (
        "wallet.read.subtree_roots_in_range_v1",
        "subtree_roots_in_range",
    ),
    ("wallet.read.transaction_by_id_v1", "transaction_by_id"),
    ("wallet.read.server_info_v1", "server_info"),
    ("wallet.broadcast.transaction_v1", "broadcast_transaction"),
    ("wallet.events.chain_v1", "chain_events"),
    ("wallet.snapshot.mempool_v1", "mempool_snapshot"),
    ("wallet.events.mempool_v1", "mempool_events"),
    (
        "wallet.address.transparent_utxos_v1",
        "transparent_address_utxos",
    ),
    (
        "wallet.address.transparent_history_v1",
        "transparent_address_tx_ids_in_range",
    ),
    ("derive.explorer.ready_v1", "explorer_server_info"),
];

#[test]
fn every_advertised_capability_has_a_documented_method_mapping() {
    for capability in ZINDER_CAPABILITIES {
        let mapping = EXPECTED_METHOD_NAMES
            .iter()
            .find(|(advertised, _)| advertised == capability);

        assert!(
            mapping.is_some(),
            "capability {capability} is advertised in ZINDER_CAPABILITIES but has no \
             ChainIndex method mapping in EXPECTED_METHOD_NAMES; add a row to \
             crates/zinder-client/tests/integration/capability_coverage.rs in the same \
             change as the capability string."
        );
    }
}

#[test]
fn capability_coverage_table_does_not_reference_retired_capabilities() {
    for (capability, method_name) in EXPECTED_METHOD_NAMES {
        assert!(
            ZINDER_CAPABILITIES.contains(capability),
            "EXPECTED_METHOD_NAMES references capability {capability} (method \
             {method_name}) that is no longer in ZINDER_CAPABILITIES; remove the row \
             when retiring a capability."
        );
    }
}

/// Compile-time existence check for [`ChainIndex`] methods.
///
/// The function body references each trait method named in
/// `EXPECTED_METHOD_NAMES`; renaming or removing any of them breaks the
/// build, so the table is bound to the trait at compile time. The function
/// itself is never called.
#[allow(
    dead_code,
    reason = "compile-time existence check for ChainIndex methods named in EXPECTED_METHOD_NAMES"
)]
fn assert_wallet_chain_index_methods_compile<T: ChainIndex>() {
    let _ = T::latest_block;
    let _ = T::compact_block_at;
    let _ = T::compact_blocks_in_range;
    let _ = T::tree_state_at;
    let _ = T::latest_tree_state;
    let _ = T::subtree_roots_in_range;
    let _ = T::transaction_by_id;
    let _ = T::server_info;
    let _ = T::broadcast_transaction;
    let _ = T::chain_events;
    let _ = T::mempool_snapshot;
    let _ = T::mempool_events;
    let _ = T::transparent_address_utxos;
    let _ = T::transparent_address_tx_ids_in_range;
}
