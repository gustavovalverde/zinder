#![allow(
    missing_docs,
    reason = "Integration test names describe the capability-registry contract under test."
)]

use std::collections::BTreeSet;

use zinder_proto::capabilities::{
    AdvertisePolicy, CAPABILITIES, CapabilitySurface, ExplorerReadiness,
    WALLET_READ_FULL_BLOCK_AT_V1, WALLET_READ_FULL_BLOCK_RANGE_V1,
    WALLET_READ_TRANSACTION_BY_ID_V2, WALLET_READ_TRANSACTION_BYTES_V1,
    WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1, capabilities_for_surface,
};

fn wallet_policy(capability: &str) -> Option<AdvertisePolicy> {
    capabilities_for_surface(CapabilitySurface::Wallet)
        .find(|spec| spec.string == capability)
        .map(|spec| spec.policy)
}

#[test]
fn wallet_registry_preserves_table_order_and_unique_identifiers() {
    let wallet_capabilities: Vec<&str> = capabilities_for_surface(CapabilitySurface::Wallet)
        .map(|spec| spec.string)
        .collect();
    let registry_wallet_capabilities: Vec<&str> = CAPABILITIES
        .iter()
        .filter(|spec| spec.surface == CapabilitySurface::Wallet)
        .map(|spec| spec.string)
        .collect();
    let unique_wallet_capabilities: BTreeSet<&str> = wallet_capabilities.iter().copied().collect();

    assert!(!wallet_capabilities.is_empty());
    assert_eq!(wallet_capabilities, registry_wallet_capabilities);
    assert_eq!(
        wallet_capabilities.len(),
        unique_wallet_capabilities.len(),
        "wallet capability identifiers must be unique"
    );
}

#[test]
fn wallet_registry_records_requirements_without_resolving_runtime_support() {
    assert_eq!(
        wallet_policy(WALLET_READ_FULL_BLOCK_AT_V1),
        Some(AdvertisePolicy::RequiresBlockBlobs)
    );
    assert_eq!(
        wallet_policy(WALLET_READ_FULL_BLOCK_RANGE_V1),
        Some(AdvertisePolicy::RequiresBlockBlobs)
    );
    assert_eq!(
        wallet_policy(WALLET_READ_TRANSACTION_BYTES_V1),
        Some(AdvertisePolicy::RequiresTransactionBlobs)
    );
    assert_eq!(
        wallet_policy(WALLET_READ_TRANSACTION_BY_ID_V2),
        Some(AdvertisePolicy::AlwaysOn)
    );
    assert_eq!(
        wallet_policy(WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1),
        Some(AdvertisePolicy::RequiresUtxoSetCommitment)
    );
}

#[test]
fn wallet_only_policies_do_not_satisfy_other_surfaces() {
    let readiness = ExplorerReadiness {
        wallet_query_online: true,
        canonical_store_online: true,
        materialized_view_store_online: true,
        prevout_resolution_online: true,
        ..ExplorerReadiness::default()
    };
    assert!(!AdvertisePolicy::RequiresBlockBlobs.explorer_satisfied(readiness));
    assert!(!AdvertisePolicy::RequiresTransactionBlobs.explorer_satisfied(readiness));
    assert!(!AdvertisePolicy::RequiresBlockBlobs.ingest_satisfied(true));
    assert!(!AdvertisePolicy::RequiresTransactionBlobs.ingest_satisfied(true));
}

#[test]
fn transaction_history_policies_require_typed_projection_readiness_and_wallet_query() {
    let available = ExplorerReadiness {
        wallet_query_online: true,
        transaction_history_available: true,
        ..ExplorerReadiness::default()
    };
    assert!(AdvertisePolicy::RequiresTransactionHistory.explorer_satisfied(available));
    assert!(!AdvertisePolicy::RequiresCompleteTransactionHistory.explorer_satisfied(available));

    let complete = ExplorerReadiness {
        transaction_history_complete: true,
        ..available
    };
    assert!(AdvertisePolicy::RequiresCompleteTransactionHistory.explorer_satisfied(complete));
    let disconnected = ExplorerReadiness {
        wallet_query_online: false,
        ..complete
    };
    assert!(!AdvertisePolicy::RequiresTransactionHistory.explorer_satisfied(disconnected));
    assert!(!AdvertisePolicy::RequiresCompleteTransactionHistory.explorer_satisfied(disconnected));
}
