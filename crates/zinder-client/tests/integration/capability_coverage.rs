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
//! `explorer.*` namespace target `ExplorerQuery` server types and are checked
//! by the consuming explorer crate; this test treats them as opaque table
//! entries.

use zinder_client::{
    ChainIndex, EXPLORER_BLOCK_DETAIL_V1, EXPLORER_BLOCK_SUMMARY_V1, EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_SUMMARY_V1, EXPLORER_SEARCH_V1,
    EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_DETAIL_V1,
    EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1, EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
    WALLET_ADDRESS_TRANSPARENT_BALANCE_V1, WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
    WALLET_ADDRESS_TRANSPARENT_UTXOS_V1, WALLET_BROADCAST_TRANSACTION_V1, WALLET_EVENTS_CHAIN_V1,
    WALLET_EVENTS_MEMPOOL_V1, WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1,
    WALLET_MEMPOOL_TRANSPARENT_PREVOUTS_V1, WALLET_MEMPOOL_TRANSPARENT_SPEND_BY_OUTPOINT_V1,
    WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1, WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
    WALLET_READ_COMPACT_BLOCK_AT_V1, WALLET_READ_COMPACT_BLOCK_RANGE_V1,
    WALLET_READ_FULL_BLOCK_AT_V1, WALLET_READ_LATEST_BLOCK_V1, WALLET_READ_LATEST_TREE_STATE_V1,
    WALLET_READ_SERVER_INFO_V1, WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1,
    WALLET_READ_TRANSACTION_BY_ID_V1, WALLET_READ_TRANSPARENT_PREVOUTS_V1,
    WALLET_READ_TREE_STATE_AT_V1, WALLET_SNAPSHOT_MEMPOOL_V1, ZINDER_CAPABILITIES,
};

/// Capability-to-method coverage table. The `ChainIndex` trait surface is
/// reflected by name; the test walks `ZINDER_CAPABILITIES` and asserts every
/// entry has a corresponding row here.
const EXPECTED_METHOD_NAMES: &[(&str, &str)] = &[
    (WALLET_READ_LATEST_BLOCK_V1, "latest_block"),
    (WALLET_READ_BLOCK_ID_BY_SELECTOR_V1, "block_id_by_selector"),
    (
        WALLET_READ_BLOCK_HEADER_BY_SELECTOR_V1,
        "block_header_by_selector",
    ),
    (WALLET_READ_COMPACT_BLOCK_AT_V1, "compact_block_at"),
    (
        WALLET_READ_COMPACT_BLOCK_RANGE_V1,
        "compact_blocks_in_range",
    ),
    (WALLET_READ_FULL_BLOCK_AT_V1, "full_block_at"),
    (WALLET_READ_TREE_STATE_AT_V1, "tree_state_at"),
    (WALLET_READ_LATEST_TREE_STATE_V1, "latest_tree_state"),
    (
        WALLET_READ_SUBTREE_ROOTS_IN_RANGE_V1,
        "subtree_roots_in_range",
    ),
    (WALLET_READ_TRANSACTION_BY_ID_V1, "transaction_by_id"),
    (WALLET_READ_SERVER_INFO_V1, "server_info"),
    (WALLET_BROADCAST_TRANSACTION_V1, "broadcast_transaction"),
    (WALLET_EVENTS_CHAIN_V1, "chain_events"),
    (WALLET_SNAPSHOT_MEMPOOL_V1, "mempool_snapshot"),
    (WALLET_EVENTS_MEMPOOL_V1, "mempool_events"),
    (
        WALLET_MEMPOOL_TRANSPARENT_OUTPUTS_BY_ADDRESS_V1,
        "transparent_mempool_outputs_by_address",
    ),
    (
        WALLET_MEMPOOL_TRANSPARENT_SPEND_BY_OUTPOINT_V1,
        "transparent_mempool_spend_by_outpoint",
    ),
    (
        WALLET_MEMPOOL_TRANSPARENT_PREVOUTS_V1,
        "transparent_mempool_prevouts",
    ),
    (WALLET_READ_TRANSPARENT_PREVOUTS_V1, "transparent_prevouts"),
    (
        WALLET_ADDRESS_TRANSPARENT_UTXOS_V1,
        "transparent_address_utxos",
    ),
    (
        WALLET_ADDRESS_TRANSPARENT_HISTORY_V1,
        "transparent_address_tx_ids_in_range",
    ),
    (
        WALLET_ADDRESS_TRANSPARENT_BALANCE_V1,
        "transparent_address_balance",
    ),
    (EXPLORER_SERVER_INFO_V1, "explorer_server_info"),
    (
        EXPLORER_TRANSPARENT_ADDRESS_BALANCE_V1,
        "transparent_address_balance",
    ),
    (EXPLORER_TRANSACTION_DETAIL_V1, "transaction_detail"),
    (EXPLORER_BLOCK_SUMMARY_V1, "block_summaries_in_range"),
    (EXPLORER_BLOCK_DETAIL_V1, "block_detail"),
    (EXPLORER_SEARCH_V1, "search"),
    (EXPLORER_MEMPOOL_SUMMARY_V1, "mempool_summary"),
    (EXPLORER_MEMPOOL_ACTIVITY_V1, "mempool_activity"),
    (
        EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V1,
        "transparent_address_activity",
    ),
    (EXPLORER_FEE_SUMMARY_V1, "fee_summary"),
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
    let _ = T::block_id_by_selector;
    let _ = T::block_header_by_selector;
    let _ = T::compact_block_at;
    let _ = T::compact_blocks_in_range;
    let _ = T::full_block_at;
    let _ = T::tree_state_at;
    let _ = T::latest_tree_state;
    let _ = T::subtree_roots_in_range;
    let _ = T::transaction_by_id;
    let _ = T::server_info;
    let _ = T::broadcast_transaction;
    let _ = T::chain_events;
    let _ = T::mempool_snapshot;
    let _ = T::mempool_events;
    let _ = T::transparent_mempool_outputs_by_address;
    let _ = T::transparent_mempool_spend_by_outpoint;
    let _ = T::transparent_mempool_prevouts;
    let _ = T::transparent_prevouts;
    let _ = T::transparent_address_utxos;
    let _ = T::transparent_address_tx_ids_in_range;
    let _ = T::transparent_address_balance;
}
