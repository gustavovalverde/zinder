#![allow(
    missing_docs,
    reason = "Integration test names describe the blob-retention capability gating contract under test."
)]

use std::collections::BTreeSet;

use zinder_proto::capabilities::{
    AdvertisePolicy, CapabilitySurface, ExplorerReadiness, WALLET_READ_FULL_BLOCK_AT_V1,
    WALLET_READ_FULL_BLOCK_RANGE_V1, WALLET_READ_TRANSACTION_BY_ID_V1,
    WALLET_READ_TRANSACTION_BYTES_V1, WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1,
    WalletAdvertiseInputs, always_on_capability_strings, capabilities_for_surface,
};

fn advertised_wallet_capabilities(
    block_blobs_retained: bool,
    transaction_blobs_retained: bool,
) -> BTreeSet<&'static str> {
    let inputs = WalletAdvertiseInputs {
        broadcaster_enabled: false,
        chain_events_enabled: false,
        chain_value_pools_enabled: false,
        block_blobs_retained,
        transaction_blobs_retained,
        utxo_set_commitment_enabled: false,
    };
    capabilities_for_surface(CapabilitySurface::Wallet)
        .filter(|spec| spec.policy.wallet_satisfied(inputs))
        .map(|spec| spec.string)
        .collect()
}

#[test]
fn block_blob_policy_resolves_against_block_blob_retention() {
    let with_blobs = WalletAdvertiseInputs {
        block_blobs_retained: true,
        ..WalletAdvertiseInputs::default()
    };
    let without_blobs = WalletAdvertiseInputs {
        block_blobs_retained: false,
        ..WalletAdvertiseInputs::default()
    };
    assert!(AdvertisePolicy::RequiresBlockBlobs.wallet_satisfied(with_blobs));
    assert!(!AdvertisePolicy::RequiresBlockBlobs.wallet_satisfied(without_blobs));
}

#[test]
fn transaction_blob_policy_resolves_against_transaction_blob_retention() {
    let with_blobs = WalletAdvertiseInputs {
        transaction_blobs_retained: true,
        ..WalletAdvertiseInputs::default()
    };
    let without_blobs = WalletAdvertiseInputs {
        transaction_blobs_retained: false,
        ..WalletAdvertiseInputs::default()
    };
    assert!(AdvertisePolicy::RequiresTransactionBlobs.wallet_satisfied(with_blobs));
    assert!(!AdvertisePolicy::RequiresTransactionBlobs.wallet_satisfied(without_blobs));
}

#[test]
fn utxo_set_commitment_policy_resolves_against_operator_opt_in() {
    let enabled = WalletAdvertiseInputs {
        utxo_set_commitment_enabled: true,
        ..WalletAdvertiseInputs::default()
    };
    let disabled = WalletAdvertiseInputs::default();
    assert!(AdvertisePolicy::RequiresUtxoSetCommitment.wallet_satisfied(enabled));
    assert!(!AdvertisePolicy::RequiresUtxoSetCommitment.wallet_satisfied(disabled));
}

#[test]
fn utxo_set_commitment_capability_is_off_by_default() {
    let advertised = advertised_wallet_capabilities(false, false);
    assert!(!advertised.contains(WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1));
}

#[test]
fn blob_policies_never_advertise_on_explorer_or_ingest() {
    let readiness = ExplorerReadiness {
        wallet_query_online: true,
        canonical_store_online: true,
        derive_store_online: true,
        prevout_resolution_online: true,
        payment_disclosure_verifier_online: true,
    };
    assert!(!AdvertisePolicy::RequiresBlockBlobs.explorer_satisfied(readiness));
    assert!(!AdvertisePolicy::RequiresTransactionBlobs.explorer_satisfied(readiness));
    assert!(!AdvertisePolicy::RequiresBlockBlobs.ingest_satisfied(true));
    assert!(!AdvertisePolicy::RequiresTransactionBlobs.ingest_satisfied(true));
}

#[test]
fn retention_none_advertises_neither_full_block_nor_transaction_bytes() {
    let advertised = advertised_wallet_capabilities(false, false);
    assert!(!advertised.contains(WALLET_READ_FULL_BLOCK_AT_V1));
    assert!(!advertised.contains(WALLET_READ_FULL_BLOCK_RANGE_V1));
    assert!(!advertised.contains(WALLET_READ_TRANSACTION_BYTES_V1));
    assert!(advertised.contains(WALLET_READ_TRANSACTION_BY_ID_V1));
}

#[test]
fn retention_transactions_advertises_transaction_bytes_only() {
    let advertised = advertised_wallet_capabilities(false, true);
    assert!(!advertised.contains(WALLET_READ_FULL_BLOCK_AT_V1));
    assert!(!advertised.contains(WALLET_READ_FULL_BLOCK_RANGE_V1));
    assert!(advertised.contains(WALLET_READ_TRANSACTION_BYTES_V1));
    assert!(advertised.contains(WALLET_READ_TRANSACTION_BY_ID_V1));
}

#[test]
fn retention_all_advertises_full_block_and_transaction_bytes() {
    let advertised = advertised_wallet_capabilities(true, true);
    assert!(advertised.contains(WALLET_READ_FULL_BLOCK_AT_V1));
    assert!(advertised.contains(WALLET_READ_FULL_BLOCK_RANGE_V1));
    assert!(advertised.contains(WALLET_READ_TRANSACTION_BYTES_V1));
    assert!(advertised.contains(WALLET_READ_TRANSACTION_BY_ID_V1));
}

#[test]
fn pre_readiness_floor_drops_blob_gated_capabilities() {
    let floor: BTreeSet<&'static str> = always_on_capability_strings(CapabilitySurface::Wallet)
        .into_iter()
        .collect();
    assert!(!floor.contains(WALLET_READ_FULL_BLOCK_AT_V1));
    assert!(!floor.contains(WALLET_READ_FULL_BLOCK_RANGE_V1));
    assert!(!floor.contains(WALLET_READ_TRANSACTION_BYTES_V1));
}
