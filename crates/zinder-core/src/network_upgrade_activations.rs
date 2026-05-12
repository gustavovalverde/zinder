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

use crate::chain_epoch::{BlockHeight, Network};

/// Branch identifier value that represents the pre-Overwinter consensus rules.
///
/// The wire protocol uses `0` to mean "no branch id is associated with this
/// height" (block was mined before the first soft-fork). The wallet read API
/// preserves the same convention.
pub const PRE_OVERWINTER_BRANCH_ID: u32 = 0;

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
    pub branch_id: u32,
    /// The block height at which this upgrade's rules first apply on the
    /// node's network.
    pub activation_height: BlockHeight,
    /// The upgrade's canonical name as reported by the node (for example
    /// `"Sapling"`, `"NU5"`, `"NU6"`). Carried verbatim so unknown future
    /// upgrades remain serviceable without a Zinder code change.
    pub name: String,
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
        branch_id: u32,
    },
}

impl fmt::Display for NetworkUpgradeActivationsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBranchId { branch_id } => write!(
                formatter,
                "duplicate consensus branch id {branch_id:#010x} in network upgrade activations"
            ),
        }
    }
}

impl std::error::Error for NetworkUpgradeActivationsError {}

impl NetworkUpgradeActivations {
    /// Builds the activation table from an unsorted list.
    ///
    /// Sorts activations by `activation_height` ascending. Returns
    /// [`NetworkUpgradeActivationsError::DuplicateBranchId`] if any branch
    /// id appears more than once.
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

    /// Returns the activation active at `height`: the entry with the largest
    /// `activation_height` such that `activation_height <= height`.
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
    /// [`PRE_OVERWINTER_BRANCH_ID`] when no upgrade is active yet.
    #[must_use]
    pub fn consensus_branch_id_at(&self, height: BlockHeight) -> u32 {
        self.active_at(height)
            .map_or(PRE_OVERWINTER_BRANCH_ID, |activation| activation.branch_id)
    }

    /// Returns the activation height for a given branch identifier, if
    /// advertised.
    #[must_use]
    pub fn activation_height_by_branch_id(&self, branch_id: u32) -> Option<BlockHeight> {
        self.activations
            .iter()
            .find(|activation| activation.branch_id == branch_id)
            .map(|activation| activation.activation_height)
    }

    /// Returns the activation height of the upgrade named `name`
    /// (case-insensitive), if advertised. Used by the wallet-serving
    /// backfill floor and the lightwalletd `saplingActivationHeight`
    /// response.
    #[must_use]
    pub fn activation_height_by_name(&self, name: &str) -> Option<BlockHeight> {
        self.activations
            .iter()
            .find(|activation| activation.name.eq_ignore_ascii_case(name))
            .map(|activation| activation.activation_height)
    }

    /// Returns the earliest activation a wallet-serving backfill must reach
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
                    branch_id: 0x76b8_09bb,
                    activation_height: BlockHeight::new(1),
                    name: "Sapling".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: 0xc2d6_d0b4,
                    activation_height: BlockHeight::new(2),
                    name: "NU5".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: 0xc8e7_1055,
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
        assert_eq!(current.branch_id, 0xc8e7_1055);
        assert_eq!(current.name, "NU6");
        Ok(())
    }

    #[test]
    fn consensus_branch_id_at_is_pre_overwinter_below_floor() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.consensus_branch_id_at(BlockHeight::new(0)),
            PRE_OVERWINTER_BRANCH_ID
        );
        Ok(())
    }

    #[test]
    fn consensus_branch_id_at_matches_current() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.consensus_branch_id_at(BlockHeight::new(7404)),
            0xc8e7_1055
        );
        Ok(())
    }

    #[test]
    fn activation_height_by_branch_id_round_trips() -> TestResult {
        let activations = sample_regtest_activations()?;
        assert_eq!(
            activations.activation_height_by_branch_id(0xc2d6_d0b4),
            Some(BlockHeight::new(2))
        );
        assert_eq!(
            activations.activation_height_by_branch_id(0xdead_beef),
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
                    branch_id: 0xc8e7_1055,
                    activation_height: BlockHeight::new(1),
                    name: "First".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: 0xc8e7_1055,
                    activation_height: BlockHeight::new(2),
                    name: "Second".to_owned(),
                },
            ],
        );
        assert_eq!(
            outcome,
            Err(NetworkUpgradeActivationsError::DuplicateBranchId {
                branch_id: 0xc8e7_1055,
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
                branch_id: 0xc8e7_1055,
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
                    branch_id: 0xc8e7_1055,
                    activation_height: BlockHeight::new(2),
                    name: "NU6".to_owned(),
                },
                NetworkUpgradeActivation {
                    branch_id: 0x76b8_09bb,
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
}
