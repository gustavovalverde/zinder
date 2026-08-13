use zinder_core::{Network, NetworkUpgradeActivationsFingerprintVersion};
use zinder_materialized_views::MaterializedViewStoreError;
use zinder_store::{
    CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION, CanonicalStoreConstructionIdentity,
};

/// Creates a structurally valid identity for isolated materialized-view tests.
///
/// Production composition obtains this value from an admitted canonical store.
pub(super) fn construction_identity()
-> Result<CanonicalStoreConstructionIdentity, MaterializedViewStoreError> {
    let mut encoded = [0_u8; 73];
    encoded[0] = 1;
    encoded[1..5].copy_from_slice(&Network::ZcashRegtest.id().to_be_bytes());
    encoded[5..7].copy_from_slice(
        &NetworkUpgradeActivationsFingerprintVersion::CURRENT
            .value()
            .to_be_bytes(),
    );
    encoded[39..41].copy_from_slice(&CANONICAL_CONSTRUCTION_MANIFEST_FORMAT_VERSION.to_be_bytes());
    CanonicalStoreConstructionIdentity::decode_persisted(&encoded).map_err(|source| {
        MaterializedViewStoreError::CanonicalConstructionIdentityMalformed { source }
    })
}

mod consumer_schema_versioning;
mod ironwood_migration;
mod materialized_view_preset;
mod value_pool_balance_history;
mod value_pool_flow_history;
