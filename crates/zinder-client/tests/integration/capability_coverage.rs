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
    CAPABILITIES, Capability, CapabilityDescriptor, CapabilitySurface, ChainIndex,
    EndpointBackedIndex, INGEST_WRITER_PHASE_V1, always_on_capability_strings,
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

/// Compile-time existence check for the [`ChainIndex`] base reads that back
/// the canonical and materialized-view wallet capabilities.
///
/// The function body references each base trait method a wallet capability
/// calls; renaming or removing any of them breaks the build. The function
/// itself is never called.
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
    let _ = T::full_block_at;
    let _ = T::full_blocks_in_range;
    let _ = T::tree_state_at;
    let _ = T::latest_tree_state_checkpoint;
    let _ = T::subtree_roots_in_range;
    let _ = T::transaction_by_id;
    let _ = T::transparent_outputs_by_outpoint;
    let _ = T::transparent_spends_by_outpoint;
    let _ = T::transparent_unspent_outputs_by_outpoint;
    let _ = T::transparent_address_unspent_outputs;
    let _ = T::transparent_address_tx_ids_in_range;
    let _ = T::transparent_address_balance;
}

/// Compile-time existence check for the [`EndpointBackedIndex`] methods that
/// back the endpoint-only wallet capabilities.
///
/// These methods need a live ingest-control/broadcast endpoint, so they live
/// on the extension trait; renaming or removing any of them breaks the build.
#[allow(
    dead_code,
    reason = "compile-time existence check for EndpointBackedIndex methods backing wallet capabilities"
)]
fn assert_wallet_endpoint_methods_compile<T: EndpointBackedIndex>() {
    let _ = T::server_info;
    let _ = T::broadcast_transaction;
    let _ = T::chain_events;
    let _ = T::mempool_snapshot;
    let _ = T::mempool_events;
    let _ = T::is_in_mempool;
    let _ = T::transparent_mempool_outputs_by_address;
    let _ = T::transparent_mempool_spends_by_outpoint;
    let _ = T::transparent_mempool_outputs_by_outpoint;
    let _ = T::chain_value_pools_at_tip;
}

/// The typed [`Capability`] probe maps each variant onto the same wire string
/// the [`CAPABILITIES`] table advertises, so a typed probe and the advertised
/// capability cannot drift.
#[test]
fn typed_capability_variants_match_table_strings() {
    let advertised: std::collections::BTreeSet<&str> =
        CAPABILITIES.iter().map(|spec| spec.string).collect();
    for capability in [
        Capability::Broadcast,
        Capability::ChainEvents,
        Capability::MempoolSnapshot,
        Capability::MempoolEvents,
        Capability::ChainValuePools,
        Capability::TransparentAddressBalance,
    ] {
        assert!(
            advertised.contains(capability.as_str()),
            "typed Capability::{capability:?} string {} is not in the CAPABILITIES table",
            capability.as_str()
        );
    }
}

/// The default [`CapabilityDescriptor::supports`] probe reads through to the
/// raw `has` lookup over the typed variant's wire string.
#[test]
fn capability_descriptor_supports_reads_typed_variant() {
    struct StubDescriptor {
        advertised: Vec<&'static str>,
    }
    impl CapabilityDescriptor for StubDescriptor {
        fn has(&self, capability: &str) -> bool {
            self.advertised.contains(&capability)
        }
    }

    let descriptor = StubDescriptor {
        advertised: vec![Capability::Broadcast.as_str()],
    };
    assert!(descriptor.supports(Capability::Broadcast));
    assert!(!descriptor.supports(Capability::ChainValuePools));
}
