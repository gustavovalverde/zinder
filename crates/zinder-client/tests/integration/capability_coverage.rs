//! Capability-coverage contract test.
//!
//! The single source of truth is the [`CAPABILITIES`] table in
//! `zinder-proto`. Each row binds a capability string to its surface, the
//! proto method it gates, and an advertise policy. The
//! `capability-table-vs-descriptor` guard in `zinder-proto` cross-checks the
//! table's proto-method bindings against the compiled `FileDescriptorSet`;
//! this test guards the client-facing trait surface those wallet capabilities
//! map onto.
//!
//! `assert_wallet_chain_index_methods_compile` takes a function-item
//! reference to each `ChainIndex` method a wallet capability calls. Renaming
//! or removing any of those trait methods makes this file fail to compile, so
//! the wallet capability surface stays bound to the trait at build time.

use zinder_client::{
    CAPABILITIES, CapabilitySurface, ChainIndex, INGEST_WRITER_PHASE_V1,
    always_on_capability_strings,
};

#[test]
fn every_capability_row_has_a_non_empty_string() {
    for spec in CAPABILITIES {
        assert!(
            !spec.string.is_empty(),
            "a {:?} capability row has an empty capability string",
            spec.surface
        );
    }
}

/// Regression guard for the unified-ingest wire surface.
///
/// `ingest.writer.phase_v1` must stay always-on so federated clients can rely
/// on the writer-phase vocabulary advertised by every Zinder deployment.
/// ADR-0015.
#[test]
fn unified_ingest_writer_phase_is_always_on() {
    assert!(
        always_on_capability_strings(CapabilitySurface::Ingest).contains(&INGEST_WRITER_PHASE_V1),
        "{INGEST_WRITER_PHASE_V1} must stay always-on so every ingest deployment advertises it; \
         the unified-ingest wire surface depends on this invariant."
    );
}

/// Compile-time existence check for the [`ChainIndex`] methods that back the
/// wallet capability surface.
///
/// The function body references each trait method a wallet capability calls;
/// renaming or removing any of them breaks the build. The function itself is
/// never called.
#[allow(
    dead_code,
    reason = "compile-time existence check for ChainIndex methods backing wallet capabilities"
)]
fn assert_wallet_chain_index_methods_compile<T: ChainIndex>() {
    let _ = T::latest_block;
    let _ = T::block_id_by_selector;
    let _ = T::block_header_by_selector;
    let _ = T::compact_block_at;
    let _ = T::compact_blocks_in_range;
    let _ = T::tree_state_at;
    let _ = T::latest_tree_state_checkpoint;
    let _ = T::subtree_roots_in_range;
    let _ = T::transaction_by_id;
    let _ = T::server_info;
    let _ = T::broadcast_transaction;
    let _ = T::chain_events;
    let _ = T::mempool_snapshot;
    let _ = T::mempool_events;
    let _ = T::transparent_mempool_outputs_by_address;
    let _ = T::transparent_mempool_spends_by_outpoint;
    let _ = T::transparent_mempool_outputs_by_outpoint;
    let _ = T::transparent_outputs_by_outpoint;
    let _ = T::transparent_address_unspent_outputs;
    let _ = T::transparent_address_tx_ids_in_range;
    let _ = T::transparent_address_balance;
    let _ = T::chain_value_pools_at_tip;
}
