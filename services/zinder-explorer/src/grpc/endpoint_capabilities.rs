//! Immutable `ExplorerQuery` capability derivation from composed dependencies.

use std::sync::Arc;

use zinder_materialized_views::{MaterializedViewConsumerName, MaterializedViewStore};
use zinder_proto::capabilities::{self, CapabilitySurface, capabilities_for_surface};
use zinder_store::SecondaryChainStore;

use super::endpoint_admission::AdmittedWalletQueryEndpoint;

/// Immutable capability identifiers for one finalized explorer adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ExplorerEndpointCapabilities {
    ordered_identifiers: Arc<[&'static str]>,
}

impl ExplorerEndpointCapabilities {
    /// Returns whether the finalized adapter serves `capability`.
    #[must_use]
    pub(super) fn contains(&self, capability: &str) -> bool {
        self.ordered_identifiers.contains(&capability)
    }

    /// Shares the exact immutable identifier slice with another discovery
    /// surface.
    #[must_use]
    pub(super) fn shared_identifiers(&self) -> Arc<[&'static str]> {
        Arc::clone(&self.ordered_identifiers)
    }

    pub(super) fn derive(
        canonical_store: Option<&SecondaryChainStore>,
        materialized_view_store: Option<&MaterializedViewStore>,
        network_upgrade_activations: Option<&zinder_core::NetworkUpgradeActivations>,
        wallet_endpoint: Option<&AdmittedWalletQueryEndpoint>,
    ) -> Self {
        let wallet_capabilities =
            wallet_endpoint.map(AdmittedWalletQueryEndpoint::capability_identifiers);
        Self::derive_from_composition(
            canonical_store.is_some(),
            materialized_view_store,
            network_upgrade_activations,
            wallet_capabilities,
        )
    }

    fn derive_from_composition(
        has_canonical_store: bool,
        materialized_view_store: Option<&MaterializedViewStore>,
        network_upgrade_activations: Option<&zinder_core::NetworkUpgradeActivations>,
        wallet_capabilities: Option<&[String]>,
    ) -> Self {
        let structurally_supported = capabilities_for_surface(CapabilitySurface::Explorer)
            .filter(|spec| {
                capability_requirements(spec.string, has_canonical_store).is_some_and(
                    |requirements| {
                        requirements.satisfied_by(
                            has_canonical_store,
                            materialized_view_store,
                            network_upgrade_activations,
                            wallet_capabilities,
                        )
                    },
                )
            })
            .map(|spec| spec.string)
            .collect::<Vec<_>>();
        let ordered_identifiers = structurally_supported
            .iter()
            .copied()
            .filter(|capability| field_has_admitted_carrier(capability, &structurally_supported))
            .collect::<Vec<_>>()
            .into();
        Self {
            ordered_identifiers,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExplorerCapabilityRequirements {
    requires_canonical_store: bool,
    consumers: &'static [MaterializedViewConsumerName],
    activation_evidence: ActivationEvidence,
    wallet_all_of: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivationEvidence {
    None,
    ActivationTable,
    Sapling,
}

impl ExplorerCapabilityRequirements {
    fn satisfied_by(
        self,
        has_canonical_store: bool,
        materialized_view_store: Option<&MaterializedViewStore>,
        network_upgrade_activations: Option<&zinder_core::NetworkUpgradeActivations>,
        wallet_capabilities: Option<&[String]>,
    ) -> bool {
        (!self.requires_canonical_store || has_canonical_store)
            && (self.consumers.is_empty()
                || materialized_view_store.is_some_and(|store| {
                    self.consumers
                        .iter()
                        .all(|consumer| store.has_consumer(*consumer))
                }))
            && match self.activation_evidence {
                ActivationEvidence::None => true,
                ActivationEvidence::ActivationTable => network_upgrade_activations.is_some(),
                ActivationEvidence::Sapling => {
                    network_upgrade_activations.is_some_and(|activations| {
                        activations.activation_height_by_name("Sapling").is_some()
                    })
                }
            }
            && has_all_wallet_capabilities(wallet_capabilities, self.wallet_all_of)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "One exhaustive registry-to-concrete-evidence table keeps every explorer contract auditable in one place."
)]
fn capability_requirements(
    explorer_capability: &str,
    has_canonical_store: bool,
) -> Option<ExplorerCapabilityRequirements> {
    use capabilities::{
        EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1, EXPLORER_BLOCK_DETAIL_V1,
        EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1, EXPLORER_BLOCK_PRODUCTION_SERIES_V2,
        EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1, EXPLORER_BLOCK_TRANSACTIONS_V2,
        EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1, EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1,
        EXPLORER_CHAIN_REORG_HISTORY_V1, EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
        EXPLORER_COMMITMENT_ROOT_SEARCH_V1, EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1,
        EXPLORER_FEE_SUMMARY_V1, EXPLORER_MEMPOOL_ACTIVITY_V1, EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
        EXPLORER_MEMPOOL_SNAPSHOT_V1, EXPLORER_MIGRATION_COHORTS_V1,
        EXPLORER_MIGRATION_DENOMINATIONS_V1, EXPLORER_MIGRATION_OVERVIEW_V1,
        EXPLORER_NETWORK_UPGRADE_STATUS_V1, EXPLORER_PAID_FEE_DISTRIBUTION_V1, EXPLORER_SEARCH_V1,
        EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2,
        EXPLORER_TRANSACTION_DETAIL_V4, EXPLORER_TRANSACTION_FEES_V1,
        EXPLORER_TRANSACTION_HISTORY_V1, EXPLORER_TRANSACTION_HISTORY_V2,
        EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1, EXPLORER_TRANSACTION_RECENT_V1,
        EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2, EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1,
        EXPLORER_UTXO_SET_COMMITMENT_V1, EXPLORER_UTXO_SET_SUMMARY_V1,
        EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1,
        EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1,
        EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1, EXPLORER_VALUE_POOL_FLOW_HISTORY_V1,
        EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1, EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1,
        EXPLORER_VALUE_POOL_SUMMARY_V1, WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
        WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1, WALLET_READ_TRANSACTION_BY_ID_V2,
        WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1,
        WALLET_READ_TRANSPARENT_UTXO_SET_SUMMARY_V1, WALLET_READ_VISIBLE_TIP_BLOCK_V1,
        WALLET_SNAPSHOT_MEMPOOL_V3,
    };
    use zinder_materialized_views::{
        BLOCK_PRODUCTION_TIME_CONSUMER_NAME, BLOCK_SUMMARY_CONSUMER_NAME,
        COMMITMENT_ROOT_SEARCH_CONSUMER_NAME, CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME,
        IRONWOOD_MIGRATION_CONSUMER_NAME, MEMPOOL_EVENT_COUNTS_CONSUMER_NAME,
        PAID_FEE_DISTRIBUTION_CONSUMER_NAME, RECENT_TRANSACTIONS_CONSUMER_NAME,
        REORG_INCIDENTS_CONSUMER_NAME, TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME,
        TRANSACTION_FEES_CONSUMER_NAME, TRANSACTION_HISTORY_CONSUMER_NAME,
        TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME, TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
        TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME, VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME,
        VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME,
    };

    let (requires_canonical_store, consumers): (bool, &'static [MaterializedViewConsumerName]) =
        match explorer_capability {
            EXPLORER_SERVER_INFO_V1
            | EXPLORER_SEARCH_V1
            | EXPLORER_MEMPOOL_SNAPSHOT_V1
            | EXPLORER_MEMPOOL_ACTIVITY_V1
            | EXPLORER_VALUE_POOL_SUMMARY_V1
            | EXPLORER_NETWORK_UPGRADE_STATUS_V1
            | EXPLORER_UTXO_SET_SUMMARY_V1
            | EXPLORER_UTXO_SET_COMMITMENT_V1 => (false, &[]),
            EXPLORER_BLOCK_PRODUCTION_SERIES_V2 => (true, &[BLOCK_SUMMARY_CONSUMER_NAME]),
            EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1 => (
                true,
                &[
                    BLOCK_PRODUCTION_TIME_CONSUMER_NAME,
                    BLOCK_SUMMARY_CONSUMER_NAME,
                    PAID_FEE_DISTRIBUTION_CONSUMER_NAME,
                ],
            ),
            EXPLORER_BLOCK_TRANSACTIONS_V2 | EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1 => {
                (true, &[BLOCK_SUMMARY_CONSUMER_NAME])
            }
            EXPLORER_COMMITMENT_ROOT_SEARCH_V1 | EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1 => {
                (true, &[COMMITMENT_ROOT_SEARCH_CONSUMER_NAME])
            }
            EXPLORER_PAID_FEE_DISTRIBUTION_V1 => (true, &[PAID_FEE_DISTRIBUTION_CONSUMER_NAME]),
            EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1
            | EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1
            | EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1 => (true, &[]),
            EXPLORER_TRANSACTION_DETAIL_V4 => (
                false,
                &[
                    TRANSACTION_FEES_CONSUMER_NAME,
                    TRANSPARENT_OUTPOINT_SPEND_CONSUMER_NAME,
                ],
            ),
            EXPLORER_BLOCK_DETAIL_V1
            | EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1
            | EXPLORER_FEE_SUMMARY_V1 => (false, &[BLOCK_SUMMARY_CONSUMER_NAME]),
            EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2 => (
                false,
                &[
                    TRANSPARENT_ADDRESS_ACTIVITY_CONSUMER_NAME,
                    TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME,
                ],
            ),
            EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1 => {
                (false, &[CONVENTIONAL_FEE_DISTRIBUTION_CONSUMER_NAME])
            }
            EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2 => {
                (false, &[TRANSACTION_COMPONENT_SUMMARY_CONSUMER_NAME])
            }
            EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1 => {
                (false, &[TRANSPARENT_ADDRESS_RANKING_CONSUMER_NAME])
            }
            EXPLORER_VALUE_POOL_FLOW_HISTORY_V1
            | EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1
            | EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1
            | EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1
            | EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1 => {
                (false, &[VALUE_POOL_FLOW_HISTORY_CONSUMER_NAME])
            }
            EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1 => {
                (false, &[VALUE_POOL_BALANCE_HISTORY_CONSUMER_NAME])
            }
            EXPLORER_CHAIN_REORG_HISTORY_V1 => (false, &[REORG_INCIDENTS_CONSUMER_NAME]),
            EXPLORER_MEMPOOL_EVENT_COUNTS_V1 => (false, &[MEMPOOL_EVENT_COUNTS_CONSUMER_NAME]),
            EXPLORER_TRANSACTION_FEES_V1 => (false, &[TRANSACTION_FEES_CONSUMER_NAME]),
            EXPLORER_TRANSACTION_RECENT_V1 => (false, &[RECENT_TRANSACTIONS_CONSUMER_NAME]),
            EXPLORER_TRANSACTION_HISTORY_V1 | EXPLORER_TRANSACTION_HISTORY_V2 => {
                (false, &[TRANSACTION_HISTORY_CONSUMER_NAME])
            }
            EXPLORER_MIGRATION_OVERVIEW_V1
            | EXPLORER_MIGRATION_COHORTS_V1
            | EXPLORER_MIGRATION_DENOMINATIONS_V1 => (false, &[IRONWOOD_MIGRATION_CONSUMER_NAME]),
            _ => return None,
        };

    let wallet_all_of: &'static [&'static str] = match explorer_capability {
        EXPLORER_SERVER_INFO_V1
        | EXPLORER_BLOCK_PRODUCTION_SERIES_V2
        | EXPLORER_BLOCK_PRODUCTION_TIME_RANGE_V1
        | EXPLORER_COMMITMENT_ROOT_SEARCH_V1
        | EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1
        | EXPLORER_TRANSACTION_FEES_V1
        | EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1
        | EXPLORER_CHAIN_REORG_HISTORY_V1
        | EXPLORER_CHAIN_DISPLACED_BLOCK_HISTORY_V1
        | EXPLORER_CHAIN_DISPLACED_BLOCK_DETAIL_V1 => &[],
        EXPLORER_TRANSACTION_DETAIL_V4 => &[
            capabilities::WALLET_READ_TRANSACTION_BY_ID_V2,
            capabilities::WALLET_READ_TRANSACTION_BYTES_V1,
        ],
        EXPLORER_BLOCK_DETAIL_V1
        | EXPLORER_BLOCK_TRANSACTIONS_V2
        | EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1 => &[
            WALLET_READ_VISIBLE_TIP_BLOCK_V1,
            WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
        ],
        EXPLORER_SEARCH_V1 => &[
            WALLET_READ_VISIBLE_TIP_BLOCK_V1,
            WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
            WALLET_READ_TRANSACTION_BY_ID_V2,
        ],
        EXPLORER_MEMPOOL_SNAPSHOT_V1 | EXPLORER_MEMPOOL_ACTIVITY_V1 => {
            &[WALLET_SNAPSHOT_MEMPOOL_V3]
        }
        EXPLORER_VALUE_POOL_SUMMARY_V1 => &[WALLET_READ_CHAIN_VALUE_POOLS_AT_TIP_V1],
        EXPLORER_UTXO_SET_SUMMARY_V1 => &[WALLET_READ_TRANSPARENT_UTXO_SET_SUMMARY_V1],
        EXPLORER_UTXO_SET_COMMITMENT_V1 => &[
            WALLET_READ_TRANSPARENT_UTXO_SET_SUMMARY_V1,
            WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1,
        ],
        EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2 => {
            if has_canonical_store {
                &[]
            } else {
                &[WALLET_READ_VISIBLE_TIP_BLOCK_V1]
            }
        }
        EXPLORER_BLOCK_ACTIVITY_DISTRIBUTION_V1
        | EXPLORER_FEE_SUMMARY_V1
        | EXPLORER_CONVENTIONAL_FEE_DISTRIBUTION_V1
        | EXPLORER_PAID_FEE_DISTRIBUTION_V1
        | EXPLORER_TRANSACTION_COMPONENT_SUMMARY_V2
        | EXPLORER_TRANSPARENT_ADDRESS_RANKING_V1
        | EXPLORER_NETWORK_UPGRADE_STATUS_V1
        | EXPLORER_VALUE_POOL_FLOW_HISTORY_V1
        | EXPLORER_VALUE_POOL_FLOW_EVENTS_IN_RANGE_V1
        | EXPLORER_VALUE_POOL_FLOW_SUMMARY_V1
        | EXPLORER_VALUE_POOL_FLOW_AMOUNT_THRESHOLD_SUMMARY_V1
        | EXPLORER_VALUE_POOL_FLOW_ROUNDED_AMOUNT_SUMMARY_V1
        | EXPLORER_VALUE_POOL_BALANCE_HISTORY_V1
        | EXPLORER_MEMPOOL_EVENT_COUNTS_V1
        | EXPLORER_TRANSACTION_RECENT_V1
        | EXPLORER_TRANSACTION_HISTORY_V1
        | EXPLORER_TRANSACTION_HISTORY_V2
        | EXPLORER_MIGRATION_OVERVIEW_V1
        | EXPLORER_MIGRATION_COHORTS_V1
        | EXPLORER_MIGRATION_DENOMINATIONS_V1 => &[WALLET_READ_VISIBLE_TIP_BLOCK_V1],
        _ => return None,
    };
    let activation_evidence = match explorer_capability {
        EXPLORER_NETWORK_UPGRADE_STATUS_V1 => ActivationEvidence::ActivationTable,
        EXPLORER_COMMITMENT_ROOT_SEARCH_V1 | EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1 => {
            ActivationEvidence::Sapling
        }
        _ => ActivationEvidence::None,
    };
    Some(ExplorerCapabilityRequirements {
        requires_canonical_store,
        consumers,
        activation_evidence,
        wallet_all_of,
    })
}

fn has_all_wallet_capabilities(wallet_capabilities: Option<&[String]>, required: &[&str]) -> bool {
    let Some(wallet_capabilities) = wallet_capabilities else {
        return required.is_empty();
    };
    required.iter().all(|required| {
        wallet_capabilities
            .binary_search_by(|advertised| advertised.as_str().cmp(required))
            .is_ok()
    })
}

fn field_carriers(capability: &str) -> Option<&'static [&'static str]> {
    match capability {
        capabilities::EXPLORER_TRANSACTION_FEES_V1 => Some(&[
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            capabilities::EXPLORER_TRANSACTION_HISTORY_V1,
            capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
            capabilities::EXPLORER_TRANSACTION_RECENT_V1,
            capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
        ]),
        capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1 => Some(&[
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            capabilities::EXPLORER_TRANSACTION_HISTORY_V1,
            capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
        ]),
        capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1 => {
            Some(&[capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2])
        }
        capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1 => {
            Some(&[capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1])
        }
        capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1 => {
            Some(&[capabilities::EXPLORER_UTXO_SET_SUMMARY_V1])
        }
        _ => None,
    }
}

fn field_has_admitted_carrier(capability: &str, admitted: &[&str]) -> bool {
    field_carriers(capability)
        .is_none_or(|carriers| carriers.iter().any(|carrier| admitted.contains(carrier)))
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the capability mapping under test."
    )]

    use tempfile::{TempDir, tempdir};
    use zinder_core::{BlockHash, BlockHeight, ChainEpochId};
    use zinder_materialized_views::{
        BLOCK_SUMMARY_SCHEMA, MaterializedViewConsumerSchema, MaterializedViewCoverage,
        MaterializedViewState, MaterializedViewStoreOptions, RECENT_TRANSACTIONS_SCHEMA,
        TRANSACTION_FEES_SCHEMA, TRANSACTION_HISTORY_CONSUMER_NAME, TRANSACTION_HISTORY_SCHEMA,
        TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
    };
    use zinder_store::RocksDbResourceBudget;

    use super::*;

    fn normalized(capabilities: impl IntoIterator<Item = &'static str>) -> Vec<String> {
        let mut capabilities: Vec<String> = capabilities.into_iter().map(str::to_owned).collect();
        capabilities.sort_unstable();
        capabilities.dedup();
        capabilities
    }

    fn materialized_view_store(
        consumers: &'static [MaterializedViewConsumerSchema],
    ) -> Result<(TempDir, MaterializedViewStore), Box<dyn std::error::Error>> {
        let directory = tempdir()?;
        let store = MaterializedViewStore::open(
            directory.path(),
            MaterializedViewStoreOptions {
                consumers,
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
                ..MaterializedViewStoreOptions::default()
            },
        )?;
        Ok((directory, store))
    }

    const UNSERVED_EXPLORER_CAPABILITIES: [&str; 4] = [
        capabilities::EXPLORER_BLOCK_SUMMARY_V1,
        capabilities::EXPLORER_MEMPOOL_SUMMARY_V1,
        capabilities::EXPLORER_TRANSPARENT_ADDRESS_DELTAS_V1,
        capabilities::EXPLORER_OVERVIEW_SNAPSHOT_V1,
    ];

    fn wallet_requirements_satisfied(
        explorer_capability: &str,
        wallet_capabilities: Option<&[String]>,
        has_canonical_store: bool,
    ) -> bool {
        capability_requirements(explorer_capability, has_canonical_store).is_some_and(
            |requirements| {
                has_all_wallet_capabilities(wallet_capabilities, requirements.wallet_all_of)
            },
        )
    }

    #[test]
    fn every_explorer_registry_row_is_served_or_in_the_test_local_unsupported_list() {
        let unserved: Vec<_> = capabilities_for_surface(CapabilitySurface::Explorer)
            .filter(|spec| capability_requirements(spec.string, false).is_none())
            .map(|spec| spec.string)
            .collect();

        assert_eq!(unserved, UNSERVED_EXPLORER_CAPABILITIES);
    }

    #[test]
    fn a_fully_capable_wallet_cannot_restore_an_unserved_contract() {
        let wallet_capabilities =
            normalized(capabilities_for_surface(CapabilitySurface::Wallet).map(|spec| spec.string));

        for capability in UNSERVED_EXPLORER_CAPABILITIES {
            assert!(!wallet_requirements_satisfied(
                capability,
                Some(&wallet_capabilities),
                false,
            ));
            assert!(!wallet_requirements_satisfied(
                capability,
                Some(&wallet_capabilities),
                true,
            ));
        }
    }

    #[test]
    fn transaction_detail_requirement_is_exactly_transaction_lookup_and_bytes() {
        let required_wallet_capabilities = normalized([
            capabilities::WALLET_READ_TRANSACTION_BY_ID_V2,
            capabilities::WALLET_READ_TRANSACTION_BYTES_V1,
        ]);
        assert!(wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            Some(&required_wallet_capabilities),
            false,
        ));
        assert!(wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            Some(&required_wallet_capabilities),
            true,
        ));

        let obsolete_canonical_alternative = normalized([
            capabilities::WALLET_READ_TRANSACTION_BY_ID_V2,
            capabilities::WALLET_READ_TRANSPARENT_SPENDS_V1,
        ]);
        assert!(!wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            Some(&obsolete_canonical_alternative),
            false,
        ));
        assert!(!wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
            Some(&obsolete_canonical_alternative),
            true,
        ));
    }

    #[test]
    fn transaction_detail_carrier_does_not_add_a_visible_tip_requirement_to_fee_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_directory, store) =
            materialized_view_store(&[TRANSACTION_FEES_SCHEMA, TRANSPARENT_OUTPOINT_SPEND_SCHEMA])?;
        let wallet_capabilities = normalized([
            capabilities::WALLET_READ_TRANSACTION_BY_ID_V2,
            capabilities::WALLET_READ_TRANSACTION_BYTES_V1,
        ]);

        let capabilities = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&store),
            None,
            Some(&wallet_capabilities),
        );

        assert!(capabilities.contains(capabilities::EXPLORER_TRANSACTION_DETAIL_V4));
        assert!(capabilities.contains(capabilities::EXPLORER_TRANSACTION_FEES_V1));
        Ok(())
    }

    #[test]
    fn transparent_address_activity_uses_canonical_or_wallet_epoch_source() {
        let visible_tip = normalized([capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1]);
        assert!(wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
            Some(&visible_tip),
            false,
        ));
        assert!(!wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
            None,
            false,
        ));
        assert!(wallet_requirements_satisfied(
            capabilities::EXPLORER_TRANSPARENT_ADDRESS_ACTIVITY_V2,
            None,
            true,
        ));
    }

    #[test]
    fn transaction_history_support_is_structural_for_both_versions()
    -> Result<(), Box<dyn std::error::Error>> {
        use zinder_materialized_views::TRANSACTION_HISTORY_CONSUMER_NAME;

        for capability in [
            capabilities::EXPLORER_TRANSACTION_HISTORY_V1,
            capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
        ] {
            let requirements = capability_requirements(capability, false)
                .ok_or("history contract is not served")?;
            assert_eq!(requirements.consumers, &[TRANSACTION_HISTORY_CONSUMER_NAME]);
            assert_eq!(
                requirements.wallet_all_of,
                &[capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1],
            );
        }
        Ok(())
    }

    #[test]
    fn activation_dependent_contracts_require_precise_admitted_evidence()
    -> Result<(), Box<dyn std::error::Error>> {
        let network_upgrade_status =
            capability_requirements(capabilities::EXPLORER_NETWORK_UPGRADE_STATUS_V1, true)
                .ok_or("network-upgrade status is not served")?;
        assert_eq!(
            network_upgrade_status.activation_evidence,
            ActivationEvidence::ActivationTable,
        );
        for capability in [
            capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1,
            capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
        ] {
            let requirements = capability_requirements(capability, true)
                .ok_or("commitment-root contract is not served")?;
            assert_eq!(
                requirements.activation_evidence,
                ActivationEvidence::Sapling
            );
        }
        Ok(())
    }

    #[test]
    fn field_capabilities_name_their_exact_response_carriers() {
        assert_eq!(
            field_carriers(capabilities::EXPLORER_TRANSACTION_FEES_V1),
            Some(
                &[
                    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
                    capabilities::EXPLORER_TRANSACTION_HISTORY_V1,
                    capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
                    capabilities::EXPLORER_TRANSACTION_RECENT_V1,
                    capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2,
                ][..],
            ),
        );
        assert_eq!(
            field_carriers(capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1,),
            Some(
                &[
                    capabilities::EXPLORER_TRANSACTION_DETAIL_V4,
                    capabilities::EXPLORER_TRANSACTION_HISTORY_V1,
                    capabilities::EXPLORER_TRANSACTION_HISTORY_V2,
                ][..],
            ),
        );
        assert_eq!(
            field_carriers(capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1),
            Some(&[capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2][..]),
        );
        assert_eq!(
            field_carriers(capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1),
            Some(&[capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1][..]),
        );
        assert_eq!(
            field_carriers(capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1),
            Some(&[capabilities::EXPLORER_UTXO_SET_SUMMARY_V1][..]),
        );
    }

    #[test]
    fn a_field_capability_without_an_admitted_carrier_is_removed() {
        assert!(!field_has_admitted_carrier(
            capabilities::EXPLORER_TRANSACTION_FEES_V1,
            &[capabilities::EXPLORER_SERVER_INFO_V1],
        ));
        assert!(field_has_admitted_carrier(
            capabilities::EXPLORER_TRANSACTION_FEES_V1,
            &[capabilities::EXPLORER_TRANSACTION_HISTORY_V2],
        ));
        assert!(field_has_admitted_carrier(
            capabilities::EXPLORER_SERVER_INFO_V1,
            &[],
        ));
    }

    #[test]
    fn history_and_recent_compositions_do_not_claim_uncomposed_fee_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let wallet_capabilities = normalized([capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1]);
        let (_history_directory, history_store) =
            materialized_view_store(&[TRANSACTION_HISTORY_SCHEMA])?;
        let history = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&history_store),
            None,
            Some(&wallet_capabilities),
        );
        assert!(history.contains(capabilities::EXPLORER_TRANSACTION_HISTORY_V1));
        assert!(history.contains(capabilities::EXPLORER_TRANSACTION_HISTORY_V2));
        assert!(!history.contains(capabilities::EXPLORER_TRANSACTION_FEES_V1));
        assert!(!history.contains(capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1));

        let (_recent_directory, recent_store) =
            materialized_view_store(&[RECENT_TRANSACTIONS_SCHEMA])?;
        let recent = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&recent_store),
            None,
            Some(&wallet_capabilities),
        );
        assert!(recent.contains(capabilities::EXPLORER_TRANSACTION_RECENT_V1));
        assert!(!recent.contains(capabilities::EXPLORER_TRANSACTION_FEES_V1));
        Ok(())
    }

    #[test]
    fn composed_fee_and_intrinsic_fields_require_a_real_carrier()
    -> Result<(), Box<dyn std::error::Error>> {
        let wallet_capabilities = normalized([capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1]);
        let (_fees_directory, fees_store) = materialized_view_store(&[TRANSACTION_FEES_SCHEMA])?;
        let fees_without_carrier = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&fees_store),
            None,
            Some(&wallet_capabilities),
        );
        assert!(!fees_without_carrier.contains(capabilities::EXPLORER_TRANSACTION_FEES_V1));

        let (_history_directory, history_store) =
            materialized_view_store(&[TRANSACTION_HISTORY_SCHEMA, TRANSACTION_FEES_SCHEMA])?;
        let history_with_fees = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&history_store),
            None,
            Some(&wallet_capabilities),
        );
        assert!(history_with_fees.contains(capabilities::EXPLORER_TRANSACTION_FEES_V1));

        let intrinsic_with_history = ExplorerEndpointCapabilities::derive_from_composition(
            true,
            Some(&history_store),
            None,
            Some(&wallet_capabilities),
        );
        assert!(
            intrinsic_with_history
                .contains(capabilities::EXPLORER_TRANSACTION_INTRINSIC_VALUE_BALANCES_V1)
        );
        Ok(())
    }

    #[test]
    fn block_field_requirements_match_their_carrier_except_for_fee_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let wallet_capabilities = normalized([
            capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1,
            capabilities::WALLET_READ_BLOCK_ID_BY_SELECTOR_V1,
        ]);
        let (_directory, block_store) = materialized_view_store(&[BLOCK_SUMMARY_SCHEMA])?;
        let block = ExplorerEndpointCapabilities::derive_from_composition(
            true,
            Some(&block_store),
            None,
            Some(&wallet_capabilities),
        );
        assert!(block.contains(capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2));
        assert!(block.contains(capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1));
        assert!(!block.contains(capabilities::EXPLORER_TRANSACTION_FEES_V1));
        assert_eq!(
            capability_requirements(capabilities::EXPLORER_BLOCK_TRANSACTIONS_V2, true,),
            capability_requirements(
                capabilities::EXPLORER_BLOCK_FINAL_NOTE_COMMITMENT_ROOTS_V1,
                true,
            ),
        );
        Ok(())
    }

    #[test]
    fn utxo_commitment_requires_the_wallet_field_capability() {
        let summary_only = normalized([capabilities::WALLET_READ_TRANSPARENT_UTXO_SET_SUMMARY_V1]);
        let base = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            None,
            None,
            Some(&summary_only),
        );
        assert!(base.contains(capabilities::EXPLORER_UTXO_SET_SUMMARY_V1));
        assert!(!base.contains(capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1));

        let summary_with_commitment = normalized([
            capabilities::WALLET_READ_TRANSPARENT_UTXO_SET_SUMMARY_V1,
            capabilities::WALLET_READ_TRANSPARENT_UTXO_SET_COMMITMENT_V1,
        ]);
        let committed = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            None,
            None,
            Some(&summary_with_commitment),
        );
        assert!(committed.contains(capabilities::EXPLORER_UTXO_SET_COMMITMENT_V1));
    }

    #[test]
    fn displaced_root_field_requirements_exactly_match_root_search() {
        assert_eq!(
            capability_requirements(capabilities::EXPLORER_COMMITMENT_ROOT_SEARCH_V1, true),
            capability_requirements(
                capabilities::EXPLORER_COMMITMENT_ROOT_DISPLACED_MATCHES_V1,
                true,
            ),
        );
    }

    #[test]
    fn frozen_history_capabilities_do_not_change_with_materialization_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let wallet_capabilities = normalized([capabilities::WALLET_READ_VISIBLE_TIP_BLOCK_V1]);
        let (_directory, history_store) = materialized_view_store(&[TRANSACTION_HISTORY_SCHEMA])?;
        let capabilities = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&history_store),
            None,
            Some(&wallet_capabilities),
        );
        let frozen = capabilities.shared_identifiers();
        assert!(capabilities.contains(capabilities::EXPLORER_TRANSACTION_HISTORY_V2));

        let partial = MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(7),
            tip_height: BlockHeight::new(20),
            tip_hash: BlockHash::from_bytes([0x20; 32]),
            revision: 1,
            coverage: None,
        };
        history_store.put_consumer_state(TRANSACTION_HISTORY_CONSUMER_NAME, partial)?;
        assert!(Arc::ptr_eq(&frozen, &capabilities.shared_identifiers()));

        let full = MaterializedViewState {
            revision: 2,
            coverage: Some(MaterializedViewCoverage {
                complete_from_height: BlockHeight::new(1),
                complete_through_height: partial.tip_height,
                complete_through_hash: partial.tip_hash,
            }),
            ..partial
        };
        history_store.put_consumer_state(TRANSACTION_HISTORY_CONSUMER_NAME, full)?;
        assert!(Arc::ptr_eq(&frozen, &capabilities.shared_identifiers()));

        let rederived = ExplorerEndpointCapabilities::derive_from_composition(
            false,
            Some(&history_store),
            None,
            Some(&wallet_capabilities),
        );
        assert_eq!(capabilities, rederived);
        Ok(())
    }
}
