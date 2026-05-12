//! Per-network consensus-upgrade schedule, sourced from the running node.
//!
//! The schedule answers three questions Zinder repeatedly needs on every
//! network (mainnet, testnet, regtest, custom testnets): which upgrades are
//! activated, at what heights, and what is the active consensus branch id at a
//! given height. Mainnet and testnet have stable answers baked into upstream
//! zebra-chain; regtest and custom testnets are operator-configured and must
//! be discovered from the running node. To keep one path correct in all cases,
//! Zinder treats the running node as the source of truth and caches the
//! result in this owned type ([`docs/architecture/chain-ingestion.md`][cha]).
//!
//! Callers receive an [`Arc<NetworkUpgradeSchedule>`] from process startup and
//! query it directly; no static singletons, no library-default fallbacks.
//!
//! [cha]: https://github.com/zcashfoundation/zinder/blob/main/docs/architecture/chain-ingestion.md

use std::collections::BTreeSet;
use std::fmt;

use crate::chain_epoch::{BlockHeight, Network};

/// Branch identifier value that represents the pre-Overwinter consensus rules.
///
/// The wire protocol uses `0` to mean "no branch id is associated with this
/// height" (block was mined before the first soft-fork). The wallet read API
/// preserves the same convention.
pub const PRE_OVERWINTER_BRANCH_ID: u32 = 0;

/// Concrete schedule of network upgrades active on a given Zcash network,
/// as advertised by the running node.
///
/// Build with [`NetworkUpgradeSchedule::new`] (in-memory construction) or via
/// the `fetch_network_upgrade_schedule` helper in `zinder-source`, which
/// parses Zebra's `getblockchaininfo.upgrades` response. Once constructed the
/// schedule is immutable; create a new one to reflect operator changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkUpgradeSchedule {
    network: Network,
    /// Sorted by `activation_height` ascending. Branch ids are unique.
    activations: Vec<NetworkUpgradeActivation>,
}

/// A single entry in a [`NetworkUpgradeSchedule`]: one upgrade and the height
/// at which the running node activates it.
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
/// [`NetworkUpgradeSchedule`].
#[derive(Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NetworkUpgradeScheduleError {
    /// Two activations advertised the same `branch_id`. Branch identifiers
    /// are global, so this indicates a malformed node response.
    DuplicateBranchId {
        /// The branch identifier that appeared more than once.
        branch_id: u32,
    },
}

impl fmt::Display for NetworkUpgradeScheduleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateBranchId { branch_id } => write!(
                formatter,
                "duplicate consensus branch id {branch_id:#010x} in upgrade schedule"
            ),
        }
    }
}

impl std::error::Error for NetworkUpgradeScheduleError {}

impl NetworkUpgradeSchedule {
    /// Builds a schedule from an unsorted list of activations.
    ///
    /// Sorts activations by `activation_height` ascending. Returns
    /// [`NetworkUpgradeScheduleError::DuplicateBranchId`] if any branch id
    /// appears more than once.
    pub fn new(
        network: Network,
        mut activations: Vec<NetworkUpgradeActivation>,
    ) -> Result<Self, NetworkUpgradeScheduleError> {
        let mut seen = BTreeSet::new();
        for activation in &activations {
            if !seen.insert(activation.branch_id) {
                return Err(NetworkUpgradeScheduleError::DuplicateBranchId {
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

    /// The network this schedule describes.
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// All activations in this schedule, sorted by activation height ascending.
    #[must_use]
    pub fn activations(&self) -> &[NetworkUpgradeActivation] {
        &self.activations
    }

    /// Returns the upgrade active at `height`: the entry with the largest
    /// `activation_height` such that `activation_height <= height`.
    ///
    /// Returns `None` if no activation in the schedule has yet activated at
    /// `height` (pre-Overwinter heights, when the node only reports
    /// post-Overwinter upgrades).
    #[must_use]
    pub fn current_at(&self, height: BlockHeight) -> Option<&NetworkUpgradeActivation> {
        self.activations
            .iter()
            .rev()
            .find(|activation| activation.activation_height.value() <= height.value())
    }

    /// Returns the consensus branch identifier active at `height`, or
    /// [`PRE_OVERWINTER_BRANCH_ID`] when no upgrade is active yet.
    #[must_use]
    pub fn consensus_branch_id_at(&self, height: BlockHeight) -> u32 {
        self.current_at(height)
            .map_or(PRE_OVERWINTER_BRANCH_ID, |activation| activation.branch_id)
    }

    /// Returns the activation height for a given branch identifier, if the
    /// schedule advertises it.
    #[must_use]
    pub fn activation_height_of_branch(&self, branch_id: u32) -> Option<BlockHeight> {
        self.activations
            .iter()
            .find(|activation| activation.branch_id == branch_id)
            .map(|activation| activation.activation_height)
    }

    /// Returns the activation height of the upgrade named `name`
    /// (case-insensitive), if the schedule advertises it. Used by the
    /// wallet-serving backfill floor and the lightwalletd
    /// `saplingActivationHeight` response.
    #[must_use]
    pub fn activation_height_of(&self, name: &str) -> Option<BlockHeight> {
        self.activations
            .iter()
            .find(|activation| activation.name.eq_ignore_ascii_case(name))
            .map(|activation| activation.activation_height)
    }

    /// Returns the earliest height a wallet-serving backfill must reach to
    /// serve lightwalletd clients on this network.
    ///
    /// Defined as the lower of the Sapling and NU5 activation heights when
    /// both are advertised; the single advertised one otherwise; `None` when
    /// neither is advertised (a malformed node response on supported
    /// networks).
    #[must_use]
    pub fn wallet_serving_floor(&self) -> Option<BlockHeight> {
        let sapling = self.activation_height_of("Sapling");
        let nu5 = self.activation_height_of("NU5");
        match (sapling, nu5) {
            (Some(sapling), Some(nu5)) => Some(BlockHeight::new(sapling.value().min(nu5.value()))),
            (Some(only), None) | (None, Some(only)) => Some(only),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_regtest_schedule() -> Result<NetworkUpgradeSchedule, NetworkUpgradeScheduleError> {
        NetworkUpgradeSchedule::new(
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
    fn current_at_returns_none_before_any_activation() -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        assert!(schedule.current_at(BlockHeight::new(0)).is_none());
        Ok(())
    }

    #[test]
    fn current_at_returns_highest_active_upgrade() -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        let Some(current) = schedule.current_at(BlockHeight::new(7404)) else {
            panic_test("regtest tip must have at least one activation at or below");
        };
        assert_eq!(current.branch_id, 0xc8e7_1055);
        assert_eq!(current.name, "NU6");
        Ok(())
    }

    #[test]
    fn consensus_branch_id_at_is_pre_overwinter_below_floor()
    -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        assert_eq!(
            schedule.consensus_branch_id_at(BlockHeight::new(0)),
            PRE_OVERWINTER_BRANCH_ID
        );
        Ok(())
    }

    #[test]
    fn consensus_branch_id_at_matches_current() -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        assert_eq!(
            schedule.consensus_branch_id_at(BlockHeight::new(7404)),
            0xc8e7_1055
        );
        Ok(())
    }

    #[test]
    fn activation_height_of_branch_round_trips() -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        assert_eq!(
            schedule.activation_height_of_branch(0xc2d6_d0b4),
            Some(BlockHeight::new(2))
        );
        assert_eq!(schedule.activation_height_of_branch(0xdead_beef), None);
        Ok(())
    }

    #[test]
    fn activation_height_of_name_is_case_insensitive() -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        assert_eq!(
            schedule.activation_height_of("sapling"),
            Some(BlockHeight::new(1))
        );
        assert_eq!(
            schedule.activation_height_of("SAPLING"),
            Some(BlockHeight::new(1))
        );
        Ok(())
    }

    #[test]
    fn new_rejects_duplicate_branch_ids() {
        let outcome = NetworkUpgradeSchedule::new(
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
            Err(NetworkUpgradeScheduleError::DuplicateBranchId {
                branch_id: 0xc8e7_1055,
            })
        );
    }

    #[test]
    fn wallet_serving_floor_prefers_earlier_of_sapling_and_nu5()
    -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = sample_regtest_schedule()?;
        // Sapling at 1, NU5 at 2 → floor is 1.
        assert_eq!(schedule.wallet_serving_floor(), Some(BlockHeight::new(1)));
        Ok(())
    }

    #[test]
    fn wallet_serving_floor_returns_none_when_neither_present()
    -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = NetworkUpgradeSchedule::new(
            Network::ZcashRegtest,
            vec![NetworkUpgradeActivation {
                branch_id: 0xc8e7_1055,
                activation_height: BlockHeight::new(2),
                name: "NU6".to_owned(),
            }],
        )?;
        assert_eq!(schedule.wallet_serving_floor(), None);
        Ok(())
    }

    #[allow(
        clippy::panic,
        reason = "Test failure path; emitted as a panic so the test framework reports a descriptive message."
    )]
    fn panic_test(message: &str) -> ! {
        panic!("{message}")
    }

    #[test]
    fn new_sorts_activations_by_height() -> Result<(), NetworkUpgradeScheduleError> {
        let schedule = NetworkUpgradeSchedule::new(
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
        let names: Vec<&str> = schedule
            .activations()
            .iter()
            .map(|activation| activation.name.as_str())
            .collect();
        assert_eq!(names, vec!["Sapling", "NU6"]);
        Ok(())
    }
}
