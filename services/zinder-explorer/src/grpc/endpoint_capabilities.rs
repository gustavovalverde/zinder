//! Immutable Explorer capability allocation for one admitted endpoint.
//!
//! The protocol registry owns stable vocabulary, but it deliberately does not
//! decide whether an Explorer composition can serve a row. This module owns
//! the one allocation made after Wallet admission and local consumer identity
//! checks; every request reads the frozen result rather than rediscovering
//! mutable store or Node state.

use std::sync::Arc;

use zinder_core::NetworkUpgradeActivations;
use zinder_materialized_views::{
    BLOCK_SUMMARY_CONSUMER_NAME, IRONWOOD_MIGRATION_CONSUMER_NAME,
    MEMPOOL_EVENT_COUNTS_CONSUMER_NAME, MaterializedViewStore, RECENT_TRANSACTIONS_CONSUMER_NAME,
    REORG_INCIDENTS_CONSUMER_NAME, TRANSACTION_FEES_CONSUMER_NAME,
};
use zinder_proto::capabilities::{
    EXPLORER_BLOCK_SUMMARY_V2, EXPLORER_CHAIN_REORG_HISTORY_V1, EXPLORER_FEE_SUMMARY_V1,
    EXPLORER_MEMPOOL_EVENT_COUNTS_V1, EXPLORER_MIGRATION_COHORTS_V1,
    EXPLORER_MIGRATION_DENOMINATIONS_V1, EXPLORER_MIGRATION_OVERVIEW_V1,
    EXPLORER_NETWORK_UPGRADE_STATUS_V1, EXPLORER_SERVER_INFO_V1, EXPLORER_TRANSACTION_FEES_V1,
    EXPLORER_TRANSACTION_RECENT_V1, WALLET_READ_VISIBLE_TIP_BLOCK_V1,
};

use super::endpoint_admission::AdmittedWalletQueryEndpoint;

/// Exact immutable Explorer capability set for one admitted composition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExplorerEndpointCapabilities {
    advertised_identifiers: Arc<[&'static str]>,
}

/// Concrete evidence observed while admitting one Explorer composition.
#[derive(Clone, Copy, Debug)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each boolean is one independent admitted provider fact used only to test the immutable allocation table"
)]
struct ExplorerAdmittedEvidence {
    has_visible_tip: bool,
    has_block_summary: bool,
    has_reorg_incidents: bool,
    has_recent_transactions: bool,
    has_transaction_fees: bool,
    has_mempool_event_counts: bool,
    has_ironwood_migration: bool,
    has_network_upgrade_activations: bool,
}

impl ExplorerEndpointCapabilities {
    /// Derives this composition's fixed allocation from admitted Wallet and
    /// exact locally registered consumer identities.
    pub(crate) fn from_admitted_composition(
        wallet_endpoint: &AdmittedWalletQueryEndpoint,
        materialized_view_store: &MaterializedViewStore,
        network_upgrade_activations: Option<&NetworkUpgradeActivations>,
    ) -> Self {
        let has_visible_tip = wallet_endpoint.has_capability(WALLET_READ_VISIBLE_TIP_BLOCK_V1);
        let has_block_summary = materialized_view_store.has_consumer(BLOCK_SUMMARY_CONSUMER_NAME);
        let has_reorg_incidents =
            materialized_view_store.has_consumer(REORG_INCIDENTS_CONSUMER_NAME);
        let has_recent_transactions =
            materialized_view_store.has_consumer(RECENT_TRANSACTIONS_CONSUMER_NAME);
        let has_transaction_fees =
            materialized_view_store.has_consumer(TRANSACTION_FEES_CONSUMER_NAME);
        let has_mempool_event_counts =
            materialized_view_store.has_consumer(MEMPOOL_EVENT_COUNTS_CONSUMER_NAME);
        let has_ironwood_migration =
            materialized_view_store.has_consumer(IRONWOOD_MIGRATION_CONSUMER_NAME);

        Self::from_admitted_evidence(ExplorerAdmittedEvidence {
            has_visible_tip,
            has_block_summary,
            has_reorg_incidents,
            has_recent_transactions,
            has_transaction_fees,
            has_mempool_event_counts,
            has_ironwood_migration,
            has_network_upgrade_activations: network_upgrade_activations.is_some(),
        })
    }

    fn from_admitted_evidence(evidence: ExplorerAdmittedEvidence) -> Self {
        // The ordered table is intentionally only the retained P6a.2 surface.
        // Absence is structural and remains absence; this is not an unsupported
        // capability catalogue or a registry-derived policy engine.
        let advertised_identifiers = [
            (EXPLORER_SERVER_INFO_V1, true),
            (
                EXPLORER_BLOCK_SUMMARY_V2,
                evidence.has_visible_tip && evidence.has_block_summary,
            ),
            (
                EXPLORER_CHAIN_REORG_HISTORY_V1,
                evidence.has_reorg_incidents,
            ),
            (
                EXPLORER_TRANSACTION_RECENT_V1,
                evidence.has_visible_tip
                    && evidence.has_block_summary
                    && evidence.has_recent_transactions,
            ),
            (
                EXPLORER_TRANSACTION_FEES_V1,
                evidence.has_transaction_fees
                    && evidence.has_visible_tip
                    && evidence.has_block_summary
                    && evidence.has_recent_transactions,
            ),
            (
                EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
                evidence.has_mempool_event_counts,
            ),
            (
                EXPLORER_FEE_SUMMARY_V1,
                evidence.has_visible_tip && evidence.has_block_summary,
            ),
            (
                EXPLORER_NETWORK_UPGRADE_STATUS_V1,
                evidence.has_visible_tip && evidence.has_network_upgrade_activations,
            ),
            (
                EXPLORER_MIGRATION_OVERVIEW_V1,
                evidence.has_visible_tip
                    && evidence.has_block_summary
                    && evidence.has_ironwood_migration,
            ),
            (
                EXPLORER_MIGRATION_COHORTS_V1,
                evidence.has_visible_tip
                    && evidence.has_block_summary
                    && evidence.has_ironwood_migration,
            ),
            (
                EXPLORER_MIGRATION_DENOMINATIONS_V1,
                evidence.has_visible_tip
                    && evidence.has_block_summary
                    && evidence.has_ironwood_migration,
            ),
        ]
        .into_iter()
        .filter_map(|(identifier, admitted)| admitted.then_some(identifier))
        .collect::<Vec<_>>()
        .into();

        Self {
            advertised_identifiers,
        }
    }

    /// Returns whether the exact immutable allocation admits `identifier`.
    #[must_use]
    pub(crate) fn contains(&self, identifier: &str) -> bool {
        self.advertised_identifiers.contains(&identifier)
    }

    /// Returns identifiers in the frozen endpoint order.
    #[must_use]
    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = &'static str> + '_ {
        self.advertised_identifiers.iter().copied()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the immutable allocation contract."
    )]

    use super::*;
    use zinder_proto::capabilities::{CAPABILITIES, CapabilitySurface};

    #[test]
    fn complete_admitted_evidence_allocates_exactly_the_frozen_eleven_of_forty_four() {
        let allocation =
            ExplorerEndpointCapabilities::from_admitted_evidence(ExplorerAdmittedEvidence {
                has_visible_tip: true,
                has_block_summary: true,
                has_reorg_incidents: true,
                has_recent_transactions: true,
                has_transaction_fees: true,
                has_mempool_event_counts: true,
                has_ironwood_migration: true,
                has_network_upgrade_activations: true,
            });
        let advertised: Vec<&str> = allocation.iter().collect();
        let registry_count = CAPABILITIES
            .iter()
            .filter(|spec| spec.surface == CapabilitySurface::Explorer)
            .count();

        assert_eq!(registry_count, 44);
        assert_eq!(advertised.len(), 11);
        assert_eq!(registry_count - advertised.len(), 33);
        assert_eq!(
            advertised,
            vec![
                EXPLORER_SERVER_INFO_V1,
                EXPLORER_BLOCK_SUMMARY_V2,
                EXPLORER_CHAIN_REORG_HISTORY_V1,
                EXPLORER_TRANSACTION_RECENT_V1,
                EXPLORER_TRANSACTION_FEES_V1,
                EXPLORER_MEMPOOL_EVENT_COUNTS_V1,
                EXPLORER_FEE_SUMMARY_V1,
                EXPLORER_NETWORK_UPGRADE_STATUS_V1,
                EXPLORER_MIGRATION_OVERVIEW_V1,
                EXPLORER_MIGRATION_COHORTS_V1,
                EXPLORER_MIGRATION_DENOMINATIONS_V1,
            ]
        );
    }

    #[test]
    fn missing_carrier_or_activation_omits_only_the_dependent_claim() {
        let allocation =
            ExplorerEndpointCapabilities::from_admitted_evidence(ExplorerAdmittedEvidence {
                has_visible_tip: true,
                has_block_summary: true,
                has_reorg_incidents: true,
                has_recent_transactions: false,
                has_transaction_fees: true,
                has_mempool_event_counts: true,
                has_ironwood_migration: true,
                has_network_upgrade_activations: false,
            });

        assert!(!allocation.contains(EXPLORER_TRANSACTION_RECENT_V1));
        assert!(!allocation.contains(EXPLORER_TRANSACTION_FEES_V1));
        assert!(!allocation.contains(EXPLORER_NETWORK_UPGRADE_STATUS_V1));
        assert!(allocation.contains(EXPLORER_BLOCK_SUMMARY_V2));
        assert!(allocation.contains(EXPLORER_FEE_SUMMARY_V1));
    }
}
