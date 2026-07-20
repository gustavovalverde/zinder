//! Product roles and recovery contracts for bundled projections.

use crate::{
    BLOCK_PRODUCTION_TIME_SCHEMA, BLOCK_SUMMARY_SCHEMA, COMMITMENT_ROOT_SEARCH_SCHEMA,
    CONVENTIONAL_FEE_DISTRIBUTION_SCHEMA, IRONWOOD_MIGRATION_SCHEMA, MEMPOOL_EVENT_COUNTS_SCHEMA,
    MaterializedViewConsumerSchema, PAID_FEE_DISTRIBUTION_SCHEMA, ProjectionPreset,
    RECENT_TRANSACTIONS_SCHEMA, REORG_INCIDENTS_SCHEMA, TRANSACTION_COMPONENT_SUMMARY_SCHEMA,
    TRANSACTION_FEES_SCHEMA, TRANSACTION_HISTORY_SCHEMA, TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
    TRANSPARENT_ADDRESS_DELTAS_SCHEMA, TRANSPARENT_ADDRESS_RANKING_SCHEMA,
    TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA, TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
    VALUE_POOL_BALANCE_HISTORY_SCHEMA, VALUE_POOL_FLOW_HISTORY_SCHEMA,
};

/// Product responsibility assigned to one projection identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionRole {
    /// Required to preserve wallet correctness after canonical retention.
    WalletCorrectness,
    /// Required by the supported wallet-serving read contract.
    WalletServing,
    /// Explorer, analytics, dashboard, or other optional product view.
    OptionalProductView,
}

/// Durable source used to reconstruct one projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionRecoverySource {
    /// Replays retained canonical chain events.
    CanonicalChainEvents,
    /// Replays the live tail and owns a separate historical canonical backfill.
    CanonicalBackfillAndChainEvents,
    /// Builds an initial canonical snapshot before following chain events.
    CanonicalSnapshotAndChainEvents,
    /// Reconstructs entirely from a historical canonical backfill worker.
    CanonicalBackfill,
    /// Replays writer-owned mempool events.
    MempoolEvents,
}

/// Canonical data this projection may authorize Zinder to discard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanonicalRetentionAuthority {
    /// Projection has no authority over canonical retention.
    None,
    /// Durable progress may release canonical transparent spend facts.
    TransparentSpendFacts,
}

/// Product presets that include one projection identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionPresetMembership {
    /// Projection is required by both wallet and explorer deployments.
    WalletAndExplorer,
    /// Projection is included only in the explorer workload.
    ExplorerOnly,
}

/// Complete product and recovery declaration for one durable projection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionDefinition {
    /// Durable schema identity and owned row contract.
    pub schema: MaterializedViewConsumerSchema,
    /// Product role served by the projection.
    pub role: ProjectionRole,
    /// Source used to recover projection state.
    pub recovery_source: ProjectionRecoverySource,
    /// Presets that select the projection.
    pub preset_membership: ProjectionPresetMembership,
    /// Canonical retention authority, if any.
    pub retention_authority: CanonicalRetentionAuthority,
}

impl ProjectionDefinition {
    /// Returns whether this projection belongs to `preset`.
    #[must_use]
    pub const fn included_in(self, preset: ProjectionPreset) -> bool {
        match (self.preset_membership, preset) {
            (ProjectionPresetMembership::WalletAndExplorer, _)
            | (ProjectionPresetMembership::ExplorerOnly, ProjectionPreset::Explorer) => true,
            (ProjectionPresetMembership::ExplorerOnly, ProjectionPreset::Wallet) => false,
        }
    }
}

const fn optional(
    schema: MaterializedViewConsumerSchema,
    recovery_source: ProjectionRecoverySource,
) -> ProjectionDefinition {
    ProjectionDefinition {
        schema,
        role: ProjectionRole::OptionalProductView,
        recovery_source,
        preset_membership: ProjectionPresetMembership::ExplorerOnly,
        retention_authority: CanonicalRetentionAuthority::None,
    }
}

const BUNDLED_PROJECTION_DEFINITIONS: &[ProjectionDefinition] = &[
    optional(
        BLOCK_PRODUCTION_TIME_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents,
    ),
    optional(
        BLOCK_SUMMARY_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        IRONWOOD_MIGRATION_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        COMMITMENT_ROOT_SEARCH_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents,
    ),
    optional(
        CONVENTIONAL_FEE_DISTRIBUTION_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents,
    ),
    optional(
        MEMPOOL_EVENT_COUNTS_SCHEMA,
        ProjectionRecoverySource::MempoolEvents,
    ),
    optional(
        PAID_FEE_DISTRIBUTION_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents,
    ),
    optional(
        RECENT_TRANSACTIONS_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        TRANSACTION_HISTORY_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        REORG_INCIDENTS_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        TRANSACTION_FEES_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        TRANSACTION_COMPONENT_SUMMARY_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents,
    ),
    optional(
        TRANSPARENT_ADDRESS_ACTIVITY_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        TRANSPARENT_ADDRESS_DELTAS_SCHEMA,
        ProjectionRecoverySource::CanonicalChainEvents,
    ),
    optional(
        TRANSPARENT_ADDRESS_RANKING_SCHEMA,
        ProjectionRecoverySource::CanonicalSnapshotAndChainEvents,
    ),
    ProjectionDefinition {
        schema: TRANSPARENT_ADDRESS_TRANSACTION_HISTORY_SCHEMA,
        role: ProjectionRole::WalletServing,
        recovery_source: ProjectionRecoverySource::CanonicalChainEvents,
        preset_membership: ProjectionPresetMembership::WalletAndExplorer,
        retention_authority: CanonicalRetentionAuthority::None,
    },
    ProjectionDefinition {
        schema: TRANSPARENT_OUTPOINT_SPEND_SCHEMA,
        role: ProjectionRole::WalletCorrectness,
        recovery_source: ProjectionRecoverySource::CanonicalChainEvents,
        preset_membership: ProjectionPresetMembership::WalletAndExplorer,
        retention_authority: CanonicalRetentionAuthority::TransparentSpendFacts,
    },
    optional(
        VALUE_POOL_BALANCE_HISTORY_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfill,
    ),
    optional(
        VALUE_POOL_FLOW_HISTORY_SCHEMA,
        ProjectionRecoverySource::CanonicalBackfillAndChainEvents,
    ),
];

/// Returns every bundled projection's product and recovery declaration.
#[must_use]
pub const fn bundled_projection_definitions() -> &'static [ProjectionDefinition] {
    BUNDLED_PROJECTION_DEFINITIONS
}
