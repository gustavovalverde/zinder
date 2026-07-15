//! Per-network consensus-upgrade activations, sourced from the running node.
//!
//! The activations answer three questions Zinder repeatedly needs on every
//! network (mainnet, testnet, regtest, custom testnets): which upgrades are
//! activated, at what heights, and what is the active consensus branch id at
//! a given height. Mainnet and testnet have stable answers baked into
//! upstream zebra-chain; regtest and custom testnets are operator-configured
//! and must be discovered from the running node. To keep one path correct in
//! all cases, Zinder treats the running node as the source of truth and
//! caches the result in this owned type
//! ([../../../docs/architecture/chain-ingestion.md][cha]).
//!
//! Callers receive an [`Arc<NetworkUpgradeActivations>`] from process startup
//! and query it directly; no static singletons, no library-default fallbacks.
//!
//! [cha]: ../../../docs/architecture/chain-ingestion.md

use std::collections::BTreeSet;
use std::fmt;

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::chain_epoch::{BlockHeight, Network};

const ACTIVATION_FINGERPRINT_DOMAIN: &[u8] =
    b"zinder:network-upgrade-activations:fingerprint:sha256\0";

/// Zcash consensus branch identifier (ZIP-200 §`CONSENSUS_BRANCH_ID`).
///
/// Network upgrades stamp every block and transaction with a 32-bit branch
/// identifier that anchors them to a specific set of consensus rules. Branch
/// identifiers are global protocol constants: a given upgrade carries the
/// same branch id on every network that adopts it, and Zinder treats the
/// value as opaque material discovered from the running node.
///
/// The bytes flow through the wire (`uint32`) and through `getblockchaininfo`
/// (hex string) unchanged; conversions happen at the boundary.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConsensusBranchId(u32);

impl ConsensusBranchId {
    /// Branch identifier reserved for the pre-Overwinter consensus rules.
    ///
    /// The wire protocol uses `0` to mean "no branch id is associated with
    /// this height" (block was mined before the first soft-fork). The
    /// wallet read API preserves the same convention.
    pub const PRE_OVERWINTER: Self = Self(0);

    /// Wraps a raw branch identifier.
    #[must_use]
    pub const fn new(branch_id: u32) -> Self {
        Self(branch_id)
    }

    /// Returns the raw 32-bit branch identifier.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

impl fmt::Display for ConsensusBranchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#010x}", self.0)
    }
}

impl fmt::LowerHex for ConsensusBranchId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, formatter)
    }
}

/// Concrete table of network upgrades active on a given Zcash network, as
/// advertised by the running node.
///
/// Build with [`NetworkUpgradeActivations::new`] (in-memory construction) or
/// via the `fetch_network_upgrade_activations` helper in `zinder-source`,
/// which parses Zebra's `getblockchaininfo.upgrades` response. Once
/// constructed the table is immutable; create a new one to reflect operator
/// changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkUpgradeActivations {
    network: Network,
    /// Sorted by `activation_height` ascending. Branch ids are unique.
    activations: Vec<NetworkUpgradeActivation>,
}

/// A single entry in [`NetworkUpgradeActivations`]: one upgrade and the
/// height at which the running node activates it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkUpgradeActivation {
    /// The consensus branch identifier assigned to this upgrade. Stable for
    /// the lifetime of the upgrade across all networks that adopt it.
    pub branch_id: ConsensusBranchId,
    /// The block height at which this upgrade's rules first apply on the
    /// node's network.
    pub activation_height: BlockHeight,
    /// The upgrade's canonical name as reported by the node (for example
    /// `"Sapling"`, `"NU5"`, `"NU6"`). Carried verbatim so unknown future
    /// upgrades remain serviceable without a Zinder code change.
    pub name: String,
}

/// Version of the immutable network-upgrade activation-table fingerprint.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum NetworkUpgradeActivationsFingerprintVersion {
    /// Initial domain-separated SHA-256 contract.
    V1,
}

impl NetworkUpgradeActivationsFingerprintVersion {
    /// Version emitted for newly created canonical stores.
    pub const CURRENT: Self = Self::V1;

    /// Returns the stable numeric encoding.
    #[must_use]
    pub const fn value(self) -> u16 {
        match self {
            Self::V1 => 1,
        }
    }
}

/// An encoded activation fingerprint version this binary does not support.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("unsupported network upgrade activations fingerprint version {encoded_version}")]
pub struct UnsupportedNetworkUpgradeActivationsFingerprintVersion {
    encoded_version: u16,
}

impl TryFrom<u16> for NetworkUpgradeActivationsFingerprintVersion {
    type Error = UnsupportedNetworkUpgradeActivationsFingerprintVersion;

    fn try_from(encoded_version: u16) -> Result<Self, Self::Error> {
        match encoded_version {
            1 => Ok(Self::V1),
            _ => Err(UnsupportedNetworkUpgradeActivationsFingerprintVersion { encoded_version }),
        }
    }
}

/// Immutable identity of one node-discovered activation table.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NetworkUpgradeActivationsFingerprint {
    version: NetworkUpgradeActivationsFingerprintVersion,
    bytes: [u8; 32],
}

impl NetworkUpgradeActivationsFingerprint {
    /// Reconstructs a fingerprint from an admitted durable record.
    #[must_use]
    pub const fn from_bytes(
        version: NetworkUpgradeActivationsFingerprintVersion,
        bytes: [u8; 32],
    ) -> Self {
        Self { version, bytes }
    }

    /// Returns the fingerprint algorithm version.
    #[must_use]
    pub const fn version(self) -> NetworkUpgradeActivationsFingerprintVersion {
        self.version
    }

    /// Returns the domain-separated SHA-256 bytes.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.bytes
    }
}

/// Validation failures encountered while constructing a
/// [`NetworkUpgradeActivations`].
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NetworkUpgradeActivationsError {
    /// Two activations advertised the same `branch_id`. Branch identifiers
    /// are global, so this indicates a malformed node response.
    DuplicateBranchId {
        /// The branch identifier that appeared more than once.
        branch_id: ConsensusBranchId,
    },
}

impl fmt::Display for NetworkUpgradeActivationsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBranchId { branch_id } => write!(
                formatter,
                "duplicate consensus branch id {branch_id} in network upgrade activations"
            ),
        }
    }
}

impl std::error::Error for NetworkUpgradeActivationsError {}

impl NetworkUpgradeActivations {
    /// Builds an activation table that advertises no upgrades.
    ///
    /// Convenience for callers that need a table-typed value but cannot
    /// resolve consensus branch ids themselves: the explorer plane uses
    /// this when parsing transactions whose branch id is supplied by the
    /// upstream wallet response. `consensus_branch_id_at` always returns
    /// [`ConsensusBranchId::PRE_OVERWINTER`] for an empty table.
    #[must_use]
    pub const fn empty(network: Network) -> Self {
        Self {
            network,
            activations: Vec::new(),
        }
    }

    /// Builds the activation table from an unsorted list.
    ///
    /// Stably sorts activations by `activation_height` ascending, preserving
    /// the caller's order among upgrades that share a height so [`active_at`]
    /// resolves same-height ties to the last-advertised upgrade. Returns
    /// [`NetworkUpgradeActivationsError::DuplicateBranchId`] if any branch
    /// id appears more than once.
    ///
    /// [`active_at`]: Self::active_at
    pub fn new(
        network: Network,
        mut activations: Vec<NetworkUpgradeActivation>,
    ) -> Result<Self, NetworkUpgradeActivationsError> {
        let mut seen = BTreeSet::new();
        for activation in &activations {
            if !seen.insert(activation.branch_id) {
                return Err(NetworkUpgradeActivationsError::DuplicateBranchId {
                    branch_id: activation.branch_id,
                });
            }
        }
        activations.sort_by_key(|activation| activation.activation_height.value());
        Ok(Self {
            network,
            activations,
        })
    }

    /// The network these activations describe.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// All activations, sorted by activation height ascending.
    #[must_use]
    pub fn activations(&self) -> &[NetworkUpgradeActivation] {
        &self.activations
    }

    /// Commits to the exact network and ordered activation table.
    ///
    /// Distinct activation heights are canonicalized by [`Self::new`]. The
    /// stable order of entries at the same height remains significant because
    /// it determines which branch is active at that height.
    #[must_use]
    pub fn fingerprint(
        &self,
        version: NetworkUpgradeActivationsFingerprintVersion,
    ) -> NetworkUpgradeActivationsFingerprint {
        let mut hasher = Sha256::new();
        hasher.update(ACTIVATION_FINGERPRINT_DOMAIN);
        hasher.update(version.value().to_le_bytes());
        hasher.update(self.network.id().to_le_bytes());
        hasher.update(
            u64::try_from(self.activations.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        for activation in &self.activations {
            hasher.update(activation.branch_id.value().to_le_bytes());
            hasher.update(activation.activation_height.value().to_le_bytes());
            hasher.update(
                u64::try_from(activation.name.len())
                    .unwrap_or(u64::MAX)
                    .to_le_bytes(),
            );
            hasher.update(activation.name.as_bytes());
        }
        NetworkUpgradeActivationsFingerprint {
            version,
            bytes: hasher.finalize().into(),
        }
    }

    /// Returns the activation active at `height`: the entry with the largest
    /// `activation_height` such that `activation_height <= height`. When
    /// several upgrades share that height (regtest activates every upgrade at
    /// height 1), the one advertised last by the node wins, matching the
    /// node's own consensus-branch-id at the tip.
    ///
    /// Returns `None` if no advertised activation has yet activated at
    /// `height` (pre-Overwinter heights, when the node only reports
    /// post-Overwinter upgrades).
    #[must_use]
    pub fn active_at(&self, height: BlockHeight) -> Option<&NetworkUpgradeActivation> {
        self.activations
            .iter()
            .rev()
            .find(|activation| activation.activation_height.value() <= height.value())
    }

    /// Returns the consensus branch identifier active at `height`, or
    /// [`ConsensusBranchId::PRE_OVERWINTER`] when no upgrade is active yet.
    #[must_use]
    pub fn consensus_branch_id_at(&self, height: BlockHeight) -> ConsensusBranchId {
        self.active_at(height)
            .map_or(ConsensusBranchId::PRE_OVERWINTER, |activation| {
                activation.branch_id
            })
    }

    /// Returns the activation height for a given branch identifier, if
    /// advertised.
    #[must_use]
    pub fn activation_height_by_branch_id(
        &self,
        branch_id: ConsensusBranchId,
    ) -> Option<BlockHeight> {
        self.activations
            .iter()
            .find(|activation| activation.branch_id == branch_id)
            .map(|activation| activation.activation_height)
    }

    /// Returns the activation height of the upgrade named `name`
    /// (case-insensitive), if advertised. Used by the wallet-serving
    /// bulk-catchup floor and the lightwalletd `saplingActivationHeight`
    /// response.
    #[must_use]
    pub fn activation_height_by_name(&self, name: &str) -> Option<BlockHeight> {
        self.activations
            .iter()
            .find(|activation| activation.name.eq_ignore_ascii_case(name))
            .map(|activation| activation.activation_height)
    }

    /// Returns the earliest activation a wallet-serving bulk catchup must reach
    /// to serve lightwalletd clients on this network.
    ///
    /// Defined as the earlier of the Sapling and NU5 activations when both
    /// are advertised; the single advertised one otherwise; `None` when
    /// neither is advertised (a malformed node response on supported
    /// networks).
    #[must_use]
    pub fn earliest_wallet_servable_activation(&self) -> Option<&NetworkUpgradeActivation> {
        let sapling = self.find_by_name("Sapling");
        let nu5 = self.find_by_name("NU5");
        match (sapling, nu5) {
            (Some(sapling), Some(nu5)) => {
                if sapling.activation_height.value() <= nu5.activation_height.value() {
                    Some(sapling)
                } else {
                    Some(nu5)
                }
            }
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }

    fn find_by_name(&self, name: &str) -> Option<&NetworkUpgradeActivation> {
        self.activations
            .iter()
            .find(|activation| activation.name.eq_ignore_ascii_case(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn sample_regtest_activations()
    -> Result<NetworkUpgradeActivations, NetworkUpgradeActivationsError> {
        NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0x76b8_09bb),
                    activation_height: BlockHeight::new(1),
                    name: "Sapling".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0xc2d6_d0b4),
                    activation_height: BlockHeight::new(2),
                    name: "NU5".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0xc8e7_1055),
                    activation_height: BlockHeight::new(2),
                    name: "NU6".to_owned(),
                },
            ],
        )
    }

    #[test]
    fn active_at_returns_none_before_any_activation() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert!(activations.active_at(BlockHeight::new(0)).is_none());
        Ok(())
    }

    #[test]
    fn active_at_returns_highest_active_upgrade() -> TestResult {
        let activations = sample_regtest_activations()?;
        let current = activations
            .active_at(BlockHeight::new(7404))
            .ok_or("regtest tip must have at least one activation at or below")?;
        assert_eq!(current.branch_id, ConsensusBranchId::new(0xc8e7_1055));
        assert_eq!(current.name, "NU6");
        Ok(())
    }

    #[test]
    fn consensus_branch_id_at_is_pre_overwinter_below_floor() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.consensus_branch_id_at(BlockHeight::new(0)),
            ConsensusBranchId::PRE_OVERWINTER
        );
        Ok(())
    }

    #[test]
    fn consensus_branch_id_at_matches_current() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.consensus_branch_id_at(BlockHeight::new(7404)),
            ConsensusBranchId::new(0xc8e7_1055)
        );
        Ok(())
    }

    #[test]
    fn activation_height_by_branch_id_round_trips() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.activation_height_by_branch_id(ConsensusBranchId::new(0xc2d6_d0b4)),
            Some(BlockHeight::new(2))
        );
        assert_eq!(
            activations.activation_height_by_branch_id(ConsensusBranchId::new(0xdead_beef)),
            None
        );
        Ok(())
    }

    #[test]
    fn activation_height_by_name_is_case_insensitive() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.activation_height_by_name("sapling"),
            Some(BlockHeight::new(1))
        );
        assert_eq!(
            activations.activation_height_by_name("SAPLING"),
            Some(BlockHeight::new(1))
        );
        Ok(())
    }

    #[test]
    fn new_rejects_duplicate_branch_ids() {
        let outcome = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0xc8e7_1055),
                    activation_height: BlockHeight::new(1),
                    name: "First".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0xc8e7_1055),
                    activation_height: BlockHeight::new(2),
                    name: "Second".to_owned(),
                },
            ],
        );
        assert_eq!(
            outcome,
            Err(NetworkUpgradeActivationsError::DuplicateBranchId {
                branch_id: ConsensusBranchId::new(0xc8e7_1055),
            })
        );
    }

    #[test]
    fn earliest_wallet_servable_activation_prefers_earlier_of_sapling_and_nu5() -> TestResult {
        let activations = sample_regtest_activations()?;
        let earliest = activations
            .earliest_wallet_servable_activation()
            .ok_or("Sapling at 1 must yield an earliest activation")?;
        assert_eq!(earliest.name, "Sapling");
        assert_eq!(earliest.activation_height, BlockHeight::new(1));
        Ok(())
    }

    #[test]
    fn earliest_wallet_servable_activation_returns_none_when_neither_present() -> TestResult {
        let activations = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![NetworkUpgradeActivation {
                branch_id: ConsensusBranchId::new(0xc8e7_1055),
                activation_height: BlockHeight::new(2),
                name: "NU6".to_owned(),
            }],
        )?;
        assert!(activations.earliest_wallet_servable_activation().is_none());
        Ok(())
    }

    #[test]
    fn new_sorts_activations_by_height() -> TestResult {
        let activations = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0xc8e7_1055),
                    activation_height: BlockHeight::new(2),
                    name: "NU6".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: ConsensusBranchId::new(0x76b8_09bb),
                    activation_height: BlockHeight::new(1),
                    name: "Sapling".to_owned(),
                },
            ],
        )?;
        let names: Vec<&str> = activations
            .activations()
            .iter()
            .map(|activation| activation.name.as_str())
            .collect();
        assert_eq!(names, vec!["Sapling", "NU6"]);
        Ok(())
    }

    #[test]
    fn activation_fingerprint_v1_matches_known_answer() -> TestResult {
        let fingerprint = sample_regtest_activations()?
            .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
        assert_eq!(
            hex::encode(fingerprint.as_bytes()),
            "f6a66ed2897330b0f1237d37df807d9329225b23f56c67ad30877e10dddee24a"
        );
        assert_eq!(
            fingerprint.version(),
            NetworkUpgradeActivationsFingerprintVersion::V1
        );
        Ok(())
    }

    #[test]
    fn activation_fingerprint_canonicalizes_distinct_height_input_order() -> TestResult {
        let sorted = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![
                activation(1, 1, "first"),
                activation(2, 2, "second"),
                activation(3, 3, "third"),
            ],
        )?;
        let reversed = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            sorted.activations().iter().cloned().rev().collect(),
        )?;
        assert_eq!(
            sorted.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            reversed.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1)
        );
        Ok(())
    }

    #[test]
    fn activation_fingerprint_preserves_same_height_tie_order() -> TestResult {
        let first = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![activation(1, 1, "first"), activation(2, 1, "second")],
        )?;
        let second = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![activation(2, 1, "second"), activation(1, 1, "first")],
        )?;
        assert_ne!(
            first.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            second.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1)
        );
        Ok(())
    }

    #[test]
    fn activation_fingerprint_commits_to_network_and_exact_fields() -> TestResult {
        let base = NetworkUpgradeActivations::new(
            Network::ZcashRegtest,
            vec![activation(1, 1, "Sapling")],
        )?;
        let variants = [
            NetworkUpgradeActivations::new(
                Network::ZcashTestnet,
                vec![activation(1, 1, "Sapling")],
            )?,
            NetworkUpgradeActivations::new(
                Network::ZcashRegtest,
                vec![activation(2, 1, "Sapling")],
            )?,
            NetworkUpgradeActivations::new(
                Network::ZcashRegtest,
                vec![activation(1, 2, "Sapling")],
            )?,
            NetworkUpgradeActivations::new(
                Network::ZcashRegtest,
                vec![activation(1, 1, "sapling")],
            )?,
        ];
        let base_fingerprint = base.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1);
        for variant in variants {
            assert_ne!(
                base_fingerprint,
                variant.fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1)
            );
        }
        Ok(())
    }

    #[test]
    fn activation_fingerprint_version_rejects_unknown_values() {
        assert_eq!(
            NetworkUpgradeActivationsFingerprintVersion::try_from(2),
            Err(UnsupportedNetworkUpgradeActivationsFingerprintVersion { encoded_version: 2 })
        );
    }

    #[test]
    fn empty_activation_fingerprints_commit_to_network() {
        assert_ne!(
            NetworkUpgradeActivations::empty(Network::ZcashRegtest)
                .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1),
            NetworkUpgradeActivations::empty(Network::ZcashTestnet)
                .fingerprint(NetworkUpgradeActivationsFingerprintVersion::V1)
        );
    }

    fn activation(branch_id: u32, height: u32, name: &str) -> NetworkUpgradeActivation {
        NetworkUpgradeActivation {
            branch_id: ConsensusBranchId::new(branch_id),
            activation_height: BlockHeight::new(height),
            name: name.to_owned(),
        }
    }
}
