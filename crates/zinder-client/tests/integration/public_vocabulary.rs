//! Public SDK vocabulary must remain independent of generated protobuf names.

use zinder_client::{Capability, CapabilityDescriptor, ErrorReason, ServerInfo};
use zinder_proto::capabilities::{
    WALLET_READ_FULL_BLOCK_AT_V1, WALLET_READ_FULL_BLOCK_RANGE_V1,
    WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1,
};

#[test]
fn wallet_capabilities_use_client_owned_exact_match_vocabulary() {
    for (capability, expected) in [
        (Capability::FullBlock, WALLET_READ_FULL_BLOCK_AT_V1),
        (Capability::FullBlockRange, WALLET_READ_FULL_BLOCK_RANGE_V1),
        (
            Capability::NetworkUpgradeActivations,
            WALLET_READ_NETWORK_UPGRADE_ACTIVATIONS_V1,
        ),
    ] {
        assert_eq!(capability.as_str(), expected);
    }
}

#[test]
fn future_capability_and_error_reason_values_are_preserved() {
    let capability = Capability::Unknown("wallet.read.future_v7".to_owned());
    assert_eq!(capability.as_str(), "wallet.read.future_v7");

    let reason = ErrorReason::from_wire_name("FUTURE_SERVER_REASON");
    assert_eq!(
        reason,
        ErrorReason::Unknown("FUTURE_SERVER_REASON".to_owned())
    );
    assert_eq!(reason.as_str(), "FUTURE_SERVER_REASON");
}

#[allow(
    dead_code,
    reason = "compile-time guard for the client-owned server descriptor"
)]
fn server_info_supports_typed_capabilities(server_info: &ServerInfo) {
    let _ = server_info.supports(Capability::FullBlock);
    let _ = server_info.has("wallet.read.future_v7");
}
