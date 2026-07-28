#![allow(
    missing_docs,
    reason = "Integration test names describe the capability-registry contract under test."
)]

use std::collections::BTreeSet;

use zinder_proto::capabilities::{CAPABILITIES, CapabilitySurface, capabilities_for_surface};

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
