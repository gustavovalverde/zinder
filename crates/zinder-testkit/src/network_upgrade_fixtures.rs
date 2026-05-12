//! Network-upgrade-schedule fixtures and conversions for tests.
//!
//! Splits the schedule-domain helpers out from
//! [`crate::transparent_signer`]: the signer module owns key derivation and
//! v5 transaction building, while this module owns the
//! [`zinder_core::NetworkUpgradeSchedule`] shapes that tests need to drive
//! consensus-aware code paths (`MinedDetails.consensus_branch_id`,
//! `GetLightdInfo`, transparent-signing branch-id selection).
//!
//! Two helpers live here:
//!
//! - [`sample_regtest_upgrade_schedule`] returns a hand-built schedule that
//!   matches ZFND's `z3` regtest sidecar defaults (Sapling at 1, NU5 at 2,
//!   NU6 at 2). Intended for in-process integration tests that exercise
//!   `GetLightdInfo` or `MinedDetails.consensus_branch_id` without a live
//!   node. Live tests must discover the schedule from the running node, not
//!   hard-code it here.
//! - [`local_network_from_schedule`] converts a node-discovered
//!   [`zinder_core::NetworkUpgradeSchedule`] into the
//!   [`zcash_protocol::local_consensus::LocalNetwork`] shape consumed by
//!   `TransparentTestKey::from_seed_with_local_network`. Live tests that
//!   broadcast through Zebra MUST use this; the schedule comes from
//!   `ZebraJsonRpcSource::fetch_network_upgrade_schedule()` and reflects
//!   what the running node is actually configured with. Mismatched heights
//!   produce an `incorrect consensus branch id` rejection from Zebra's
//!   mempool.

use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zinder_core::{
    BlockHeight as CoreBlockHeight, NetworkUpgradeActivation, NetworkUpgradeSchedule,
};

/// Sample regtest [`NetworkUpgradeSchedule`] for in-process tests.
///
/// Matches the activation heights ZFND's `z3` regtest sidecar is configured
/// with by default (Sapling at 1, NU5 at 2, NU6 at 2). Intended for
/// integration tests that exercise `GetLightdInfo` or
/// `MinedDetails.consensus_branch_id` without a live node. Live tests must
/// discover the schedule from the running node, not hard-code it here.
#[must_use]
#[allow(
    clippy::expect_used,
    reason = "The activation list is a hand-built fixture with unique branch ids; if construction ever fails it's a test-code bug, not a runtime condition."
)]
pub fn sample_regtest_upgrade_schedule() -> NetworkUpgradeSchedule {
    NetworkUpgradeSchedule::new(
        zinder_core::Network::ZcashRegtest,
        vec![
            NetworkUpgradeActivation {
                branch_id: 0x5ba8_1b19,
                activation_height: CoreBlockHeight::new(1),
                name: "Overwinter".to_owned(),
            },
            NetworkUpgradeActivation {
                branch_id: 0x76b8_09bb,
                activation_height: CoreBlockHeight::new(1),
                name: "Sapling".to_owned(),
            },
            NetworkUpgradeActivation {
                branch_id: 0xc2d6_d0b4,
                activation_height: CoreBlockHeight::new(2),
                name: "NU5".to_owned(),
            },
            NetworkUpgradeActivation {
                branch_id: 0xc8e7_1055,
                activation_height: CoreBlockHeight::new(2),
                name: "NU6".to_owned(),
            },
        ],
    )
    .expect("hand-built regtest schedule must have unique branch ids")
}

/// Builds a [`LocalNetwork`] from a node-discovered upgrade schedule.
///
/// Populates each consensus upgrade's activation height from the schedule
/// entry whose `name` matches (case-insensitive). Upgrades the schedule
/// does not advertise stay `None`, which `zcash_primitives` reads as
/// "not yet activated". Use this for any signing path that needs to match
/// the running node's active consensus rules. The schedule comes from
/// `ZebraJsonRpcSource::fetch_network_upgrade_schedule()` and reflects the
/// node's `getblockchaininfo.upgrades` advertisement.
#[must_use]
pub fn local_network_from_schedule(schedule: &NetworkUpgradeSchedule) -> LocalNetwork {
    let lookup = |name: &str| {
        schedule
            .activation_height_of(name)
            .map(|height| BlockHeight::from_u32(height.value()))
    };
    LocalNetwork {
        overwinter: lookup("Overwinter"),
        sapling: lookup("Sapling"),
        blossom: lookup("Blossom"),
        heartwood: lookup("Heartwood"),
        canopy: lookup("Canopy"),
        nu5: lookup("NU5"),
        nu6: lookup("NU6"),
        nu6_1: lookup("NU6.1"),
    }
}
