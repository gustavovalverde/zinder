//! Transparent-address compatibility against the configured [`Network`].
//!
//! Wallet-facing read surfaces accept transparent addresses parsed by
//! [`zebra_chain::transparent::Address`] and must reject inputs that target a
//! different network than the one the deployment indexes. The check is a
//! pure mapping from `zebra-chain`'s [`ZebraNetworkKind`] to Zinder's
//! [`Network`]; both `zinder-query` and `zinder-compat-lightwalletd` import
//! it instead of carrying their own copies.

use zebra_chain::parameters::NetworkKind as ZebraNetworkKind;
use zinder_core::Network;

/// Returns true when `address_kind` is compatible with `network`.
///
/// Zinder distinguishes regtest from testnet (`ZcashRegtest` vs
/// `ZcashTestnet`) but Zebra collapses them: a `ZebraNetworkKind::Testnet`
/// address is valid on both Zinder testnet and regtest deployments. A
/// `ZebraNetworkKind::Regtest` address is only valid on Zinder regtest.
#[must_use]
#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "Network is non_exhaustive; future variants must fail closed against transparent address validation until they are explicitly handled"
)]
pub fn transparent_address_matches_network(
    address_kind: ZebraNetworkKind,
    network: Network,
) -> bool {
    match network {
        Network::ZcashMainnet => address_kind == ZebraNetworkKind::Mainnet,
        Network::ZcashTestnet => address_kind == ZebraNetworkKind::Testnet,
        Network::ZcashRegtest => matches!(
            address_kind,
            ZebraNetworkKind::Testnet | ZebraNetworkKind::Regtest
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_address_only_matches_mainnet_network() {
        assert!(transparent_address_matches_network(
            ZebraNetworkKind::Mainnet,
            Network::ZcashMainnet
        ));
        assert!(!transparent_address_matches_network(
            ZebraNetworkKind::Mainnet,
            Network::ZcashTestnet
        ));
        assert!(!transparent_address_matches_network(
            ZebraNetworkKind::Mainnet,
            Network::ZcashRegtest
        ));
    }

    #[test]
    fn testnet_address_matches_testnet_and_regtest_networks() {
        assert!(transparent_address_matches_network(
            ZebraNetworkKind::Testnet,
            Network::ZcashTestnet
        ));
        assert!(transparent_address_matches_network(
            ZebraNetworkKind::Testnet,
            Network::ZcashRegtest
        ));
        assert!(!transparent_address_matches_network(
            ZebraNetworkKind::Testnet,
            Network::ZcashMainnet
        ));
    }

    #[test]
    fn regtest_address_only_matches_regtest_network() {
        assert!(transparent_address_matches_network(
            ZebraNetworkKind::Regtest,
            Network::ZcashRegtest
        ));
        assert!(!transparent_address_matches_network(
            ZebraNetworkKind::Regtest,
            Network::ZcashTestnet
        ));
        assert!(!transparent_address_matches_network(
            ZebraNetworkKind::Regtest,
            Network::ZcashMainnet
        ));
    }
}
