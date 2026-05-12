//! Network chain name conversions across wire dialects.
//!
//! Two dialects live here: BIP70 (used verbatim by every Zcash protocol that
//! Zinder talks to or speaks) and Zinder-native (the `zcash-mainnet`-style
//! name used in config files, native protobuf, metrics labels, and operator
//! tooling).

use crate::Network;
use crate::wire::WireDecodeError;

const ZINDER_NATIVE_DIALECT: &str = "zinder-native";

/// Encode a [`Network`] as its BIP70 chain name.
///
/// Every upstream Zcash wire surface that names the network uses this value:
///
/// - lightwalletd protocol: `LightdInfo.chainName`, `TreeState.network`
///   (vendored proto `service.proto:103,178`).
/// - Zebra JSON-RPC: `getblockchaininfo.chain`
///   (`zebra-chain/src/parameters/network.rs:87-95`,
///   `zebra-rpc/src/methods.rs:1110`).
/// - zcashd JSON-RPC: `getblockchaininfo.chain`.
///
/// Returns `"main"` for mainnet, `"test"` for both testnet and regtest. Zcash
/// regtest is implemented as a parameterization of testnet at the BIP70 layer;
/// Zebra's `NetworkKind::bip70_network_name()` collapses regtest into `"test"`
/// and lightwalletd inherits that value verbatim.
#[must_use]
pub const fn encode_bip70_chain_name(network: Network) -> &'static str {
    match network {
        Network::ZcashMainnet => "main",
        Network::ZcashTestnet | Network::ZcashRegtest => "test",
    }
}

/// Encode a [`Network`] as its Zinder-native chain name.
///
/// Used by:
///
/// - Configuration files (`[network] name = "zcash-regtest"`).
/// - Native protobuf `network_name` fields (`zinder.v1.wallet`,
///   `zinder.v1.ingest`, `zinder.v1.explorer`).
/// - Metrics labels and log fields where the network is part of the record.
///
/// The Zinder-native name distinguishes regtest from testnet, unlike
/// [`encode_bip70_chain_name`]. Use this function for any surface internal to
/// Zinder or under Zinder's control. Use [`encode_bip70_chain_name`] for
/// surfaces that follow the upstream Zcash protocol convention.
#[must_use]
pub const fn encode_zinder_native_chain_name(network: Network) -> &'static str {
    match network {
        Network::ZcashMainnet => "zcash-mainnet",
        Network::ZcashTestnet => "zcash-testnet",
        Network::ZcashRegtest => "zcash-regtest",
    }
}

/// Resolve a Zinder-native chain name string back to a [`Network`].
///
/// Inverse of [`encode_zinder_native_chain_name`]. Used by configuration
/// parsing and native protobuf deserialization to recover the network from
/// its Zinder-native string form. An unrecognized input returns
/// [`WireDecodeError::UnrecognizedString`] with the `zinder-native` dialect tag.
///
/// # Errors
///
/// Returns [`WireDecodeError::UnrecognizedString`] if the input is not one of
/// `zcash-mainnet`, `zcash-testnet`, or `zcash-regtest`.
pub fn decode_zinder_native_chain_name(input: &str) -> Result<Network, WireDecodeError> {
    match input {
        "zcash-mainnet" => Ok(Network::ZcashMainnet),
        "zcash-testnet" => Ok(Network::ZcashTestnet),
        "zcash-regtest" => Ok(Network::ZcashRegtest),
        _ => Err(WireDecodeError::UnrecognizedString {
            dialect: ZINDER_NATIVE_DIALECT,
            input: input.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const ALL_NETWORKS: [Network; 3] = [
        Network::ZcashMainnet,
        Network::ZcashTestnet,
        Network::ZcashRegtest,
    ];

    #[test]
    fn bip70_chain_name_matches_vendored_proto_doc() {
        assert_eq!(encode_bip70_chain_name(Network::ZcashMainnet), "main");
        assert_eq!(encode_bip70_chain_name(Network::ZcashTestnet), "test");
        assert_eq!(encode_bip70_chain_name(Network::ZcashRegtest), "test");
    }

    #[test]
    fn zinder_native_chain_name_matches_config_convention() {
        assert_eq!(
            encode_zinder_native_chain_name(Network::ZcashMainnet),
            "zcash-mainnet"
        );
        assert_eq!(
            encode_zinder_native_chain_name(Network::ZcashTestnet),
            "zcash-testnet"
        );
        assert_eq!(
            encode_zinder_native_chain_name(Network::ZcashRegtest),
            "zcash-regtest"
        );
    }

    #[test]
    fn zinder_native_chain_name_round_trips() -> TestResult {
        for network in ALL_NETWORKS {
            let encoded = encode_zinder_native_chain_name(network);
            let decoded = decode_zinder_native_chain_name(encoded)?;
            assert_eq!(decoded, network);
        }
        Ok(())
    }

    #[test]
    fn decode_zinder_native_chain_name_rejects_unknown_string() {
        let outcome = decode_zinder_native_chain_name("bitcoin-mainnet");
        assert!(matches!(
            outcome,
            Err(WireDecodeError::UnrecognizedString {
                dialect: "zinder-native",
                ..
            })
        ));
    }

    #[test]
    fn decode_zinder_native_chain_name_rejects_bip70_string() {
        assert!(matches!(
            decode_zinder_native_chain_name("main"),
            Err(WireDecodeError::UnrecognizedString { .. })
        ));
        assert!(matches!(
            decode_zinder_native_chain_name("test"),
            Err(WireDecodeError::UnrecognizedString { .. })
        ));
    }
}
