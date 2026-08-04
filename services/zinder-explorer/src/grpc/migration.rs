//! `ExplorerQuery.MigrationOverview`, `MigrationCohorts`, and
//! `MigrationDenominations` handlers.
//!
//! All three read the Orchard-to-Ironwood migration facts materialized by
//! `zinder_materialized_views::IronwoodMigrationConsumer` and wrap the result in the
//! cross-cutting `ExplorerFreshness` envelope. `MigrationOverview` pairs the
//! per-migration rows with the cumulative pool-totals records to report the
//! migrated-value total alongside the chain-wide two-sided pool audit;
//! `MigrationCohorts` groups the rows by shared Orchard anchor; and
//! `MigrationDenominations` bins conformant rows by the power-of-ten magnitude
//! of their Ironwood output amount. Grouping and binning run in memory over a
//! bounded range at request time; nothing beyond the consumer's two column
//! families is materialized.

use std::collections::BTreeMap;

use tonic::{Request, Response, Status};
use zinder_core::BlockHeight;
use zinder_materialized_views::{
    IRONWOOD_MIGRATION_CONSUMER_NAME, IronwoodMigrationConsumer, MaterializedViewState,
    MaterializedViewStore, MaterializedViewStoreReadSnapshot, Migration, MigrationPoolTotals,
};
use zinder_proto::capabilities::{
    EXPLORER_MIGRATION_COHORTS_V1, EXPLORER_MIGRATION_DENOMINATIONS_V1,
    EXPLORER_MIGRATION_OVERVIEW_V1,
};
use zinder_proto::v1::explorer::{
    MigrationCohort, MigrationCohortsRequest, MigrationCohortsResponse, MigrationDenominationBin,
    MigrationDenominationsRequest, MigrationDenominationsResponse, MigrationOverviewRequest,
    MigrationOverviewResponse,
};
use zinder_proto::v1::wallet::wallet_query_client::WalletQueryClient;
use zinder_runtime::AuthenticatedChannel;

use super::error::ExplorerError;
use super::freshness::{
    UpstreamObservationCache, WalletPinnedBlockSummarySnapshot, attach_upstream_observation,
    build_explorer_freshness_from_snapshot, pin_wallet_to_block_summary_snapshot,
    require_block_summary_range_coverage,
};

/// Hard cap on the block span one cohort or denomination request may cover.
///
/// Bounds a single request's materialized-view scan, mirroring the bounded-page
/// rule the other explorer range reads apply.
const MAX_MIGRATION_BLOCK_SPAN: u32 = 4096;

/// Hard cap on the migration rows one request materializes in memory.
///
/// Migrations are rare coordinated events, so this ceiling sits far above any
/// realistic count; it exists only to bound the response allocation for an
/// unbounded overview range.
const MAX_MIGRATION_ROWS_PER_REQUEST: usize = 65_536;

/// Executes one `ExplorerQuery.MigrationOverview` request.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the Wallet-pinned snapshot must span migration rows, exact totals, and response freshness"
)]
pub(crate) async fn query_migration_overview(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<MigrationOverviewRequest>,
) -> Result<Response<MigrationOverviewResponse>, Status> {
    let inner = request.into_inner();
    if let (Some(start), Some(end)) = (inner.start_height, inner.end_height)
        && end < start
    {
        return Err(ExplorerError::invalid_request("end_height must be >= start_height").into());
    }
    let (aggregate, delta, freshness) = {
        let pinned =
            pin_wallet_to_block_summary_snapshot(materialized_view_store, wallet_client).await?;
        let state = pinned.block_summary_state();
        let end_height = inner.end_height.unwrap_or_else(|| state.tip_height.value());
        let coverage_start_height = state
            .coverage
            .ok_or_else(|| {
                ExplorerError::not_materialized(
                    "block-summary materialized-view coverage has not been verified",
                )
            })?
            .complete_from_height
            .value();
        let start_height = inner.start_height.unwrap_or(coverage_start_height);
        if end_height < start_height {
            return Err(
                ExplorerError::invalid_request("end_height must be >= start_height").into(),
            );
        }
        require_block_summary_range_coverage(state, start_height, end_height)?;
        require_migration_snapshot_coherence(&pinned)?;
        let snapshot = pinned.snapshot();
        let migrations = read_migrations_snapshot(snapshot, start_height, end_height)?;
        let delta = read_range_pool_delta_snapshot(
            snapshot,
            start_height,
            end_height,
            coverage_start_height,
        )?;
        let freshness = build_explorer_freshness_from_snapshot(
            snapshot,
            EXPLORER_MIGRATION_OVERVIEW_V1,
            Some(pinned.wallet_chain_epoch().clone()),
            0,
        )?;
        (aggregate_overview(&migrations), delta, freshness)
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
    Ok(Response::new(MigrationOverviewResponse {
        freshness: Some(freshness),
        total_migrated_ironwood_zat: aggregate.total_migrated_ironwood_zat,
        migration_count: aggregate.migration_count,
        first_height: aggregate.first_height,
        last_height: aggregate.last_height,
        orchard_outflow_zat: delta.orchard_outflow_zat,
        ironwood_inflow_zat: delta.ironwood_inflow_zat,
    }))
}

/// Executes one `ExplorerQuery.MigrationCohorts` request.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the Wallet-pinned snapshot must span cohort rows and response freshness"
)]
pub(crate) async fn query_migration_cohorts(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<MigrationCohortsRequest>,
) -> Result<Response<MigrationCohortsResponse>, Status> {
    let inner = request.into_inner();
    validate_span(inner.start_height, inner.end_height)?;
    let (cohorts, stats, freshness) = {
        let pinned =
            pin_wallet_to_block_summary_snapshot(materialized_view_store, wallet_client).await?;
        require_block_summary_range_coverage(
            pinned.block_summary_state(),
            inner.start_height,
            inner.end_height,
        )?;
        require_migration_snapshot_coherence(&pinned)?;
        let snapshot = pinned.snapshot();
        let migrations = read_migrations_snapshot(snapshot, inner.start_height, inner.end_height)?;
        let (cohorts, stats) = group_cohorts(&migrations);
        let freshness = build_explorer_freshness_from_snapshot(
            snapshot,
            EXPLORER_MIGRATION_COHORTS_V1,
            Some(pinned.wallet_chain_epoch().clone()),
            0,
        )?;
        (cohorts, stats, freshness)
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
    Ok(Response::new(MigrationCohortsResponse {
        freshness: Some(freshness),
        cohorts,
        cohort_count: stats.cohorts,
        avg_member_count: stats.avg_members,
        min_member_count: stats.min_members,
        max_member_count: stats.max_members,
    }))
}

/// Executes one `ExplorerQuery.MigrationDenominations` request.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the Wallet-pinned snapshot must span denomination rows and response freshness"
)]
pub(crate) async fn query_migration_denominations(
    materialized_view_store: &MaterializedViewStore,
    wallet_client: &mut WalletQueryClient<AuthenticatedChannel>,
    upstream_observation_cache: &UpstreamObservationCache,
    request: Request<MigrationDenominationsRequest>,
) -> Result<Response<MigrationDenominationsResponse>, Status> {
    let inner = request.into_inner();
    validate_span(inner.start_height, inner.end_height)?;
    let (bins, total_tx, freshness) = {
        let pinned =
            pin_wallet_to_block_summary_snapshot(materialized_view_store, wallet_client).await?;
        require_block_summary_range_coverage(
            pinned.block_summary_state(),
            inner.start_height,
            inner.end_height,
        )?;
        require_migration_snapshot_coherence(&pinned)?;
        let snapshot = pinned.snapshot();
        let migrations = read_migrations_snapshot(snapshot, inner.start_height, inner.end_height)?;
        let (bins, total_tx) = bin_denominations(&migrations);
        let freshness = build_explorer_freshness_from_snapshot(
            snapshot,
            EXPLORER_MIGRATION_DENOMINATIONS_V1,
            Some(pinned.wallet_chain_epoch().clone()),
            0,
        )?;
        (bins, total_tx, freshness)
    };
    let freshness = attach_upstream_observation(upstream_observation_cache, freshness).await;
    Ok(Response::new(MigrationDenominationsResponse {
        freshness: Some(freshness),
        bins,
        total_tx,
    }))
}

/// Migrated-value total and boundary heights over the counted migration rows.
#[derive(Default)]
struct OverviewAggregate {
    total_migrated_ironwood_zat: u64,
    migration_count: u32,
    first_height: Option<u32>,
    last_height: Option<u32>,
}

/// Chain-wide net pool movement over a block range.
#[derive(Default)]
struct PoolDelta {
    orchard_outflow_zat: u64,
    ironwood_inflow_zat: u64,
}

/// Cross-range summary of the grouped cohorts.
#[derive(Default)]
struct CohortStats {
    cohorts: u32,
    avg_members: u32,
    min_members: u32,
    max_members: u32,
}

/// Running tally for one anchor's cohort while grouping.
#[derive(Default)]
struct CohortAccumulator {
    member_count: u32,
    total_migrated_zat: u64,
    conformant_member_count: u32,
}

fn aggregate_overview(migrations: &[Migration]) -> OverviewAggregate {
    let mut total_migrated_ironwood_zat = 0u64;
    let mut migration_count = 0u32;
    for migration in migrations {
        total_migrated_ironwood_zat =
            total_migrated_ironwood_zat.saturating_add(migration.migrated_amount_zat());
        migration_count = migration_count.saturating_add(1);
    }
    OverviewAggregate {
        total_migrated_ironwood_zat,
        migration_count,
        first_height: migrations.first().map(|migration| migration.block_height),
        last_height: migrations.last().map(|migration| migration.block_height),
    }
}

fn group_cohorts(migrations: &[Migration]) -> (Vec<MigrationCohort>, CohortStats) {
    let mut grouped: BTreeMap<[u8; 32], CohortAccumulator> = BTreeMap::new();
    for migration in migrations {
        let accumulator = grouped.entry(migration.orchard_anchor).or_default();
        accumulator.member_count = accumulator.member_count.saturating_add(1);
        accumulator.total_migrated_zat = accumulator
            .total_migrated_zat
            .saturating_add(migration.migrated_amount_zat());
        if migration.conformant {
            accumulator.conformant_member_count =
                accumulator.conformant_member_count.saturating_add(1);
        }
    }

    let mut cohorts = Vec::with_capacity(grouped.len());
    let mut min_member_count: Option<u32> = None;
    let mut max_member_count = 0u32;
    let mut total_members = 0u64;
    for (orchard_anchor, accumulator) in grouped {
        min_member_count = Some(
            min_member_count.map_or(accumulator.member_count, |smallest| {
                smallest.min(accumulator.member_count)
            }),
        );
        max_member_count = max_member_count.max(accumulator.member_count);
        total_members = total_members.saturating_add(u64::from(accumulator.member_count));
        cohorts.push(MigrationCohort {
            orchard_anchor: orchard_anchor.to_vec(),
            member_count: accumulator.member_count,
            total_migrated_zat: accumulator.total_migrated_zat,
            conformant_member_count: accumulator.conformant_member_count,
        });
    }

    let cohort_count = u32::try_from(cohorts.len()).unwrap_or(u32::MAX);
    let avg_member_count = if cohort_count == 0 {
        0
    } else {
        u32::try_from(total_members / u64::from(cohort_count)).unwrap_or(u32::MAX)
    };
    let stats = CohortStats {
        cohorts: cohort_count,
        avg_members: avg_member_count,
        min_members: min_member_count.unwrap_or(0),
        max_members: max_member_count,
    };
    (cohorts, stats)
}

fn bin_denominations(migrations: &[Migration]) -> (Vec<MigrationDenominationBin>, u32) {
    let mut binned: BTreeMap<u64, u32> = BTreeMap::new();
    let mut total_tx = 0u32;
    for migration in migrations {
        if !migration.conformant {
            continue;
        }
        let denomination_zat = denomination_floor(migration.migrated_amount_zat());
        let count = binned.entry(denomination_zat).or_insert(0);
        *count = count.saturating_add(1);
        total_tx = total_tx.saturating_add(1);
    }
    let bins = binned
        .into_iter()
        .map(|(denomination_zat, count)| MigrationDenominationBin {
            denomination_zat,
            count,
        })
        .collect();
    (bins, total_tx)
}

/// Returns the largest power of ten not exceeding `amount_zat`, or zero when
/// `amount_zat` is zero.
fn denomination_floor(amount_zat: u64) -> u64 {
    amount_zat
        .checked_ilog10()
        .and_then(|exponent| 10u64.checked_pow(exponent))
        .unwrap_or(0)
}

fn read_range_pool_delta_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    start_height: u32,
    end_height: u32,
    coverage_start_height: u32,
) -> Result<PoolDelta, Status> {
    let end_totals = require_exact_pool_totals(
        read_pool_totals_at_or_before_snapshot(snapshot, end_height)?,
        end_height,
        "end",
    )?;
    let baseline = if start_height <= coverage_start_height {
        None
    } else {
        let previous = start_height.checked_sub(1).ok_or_else(|| {
            ExplorerError::not_materialized("migration range baseline underflowed")
        })?;
        Some(require_exact_pool_totals(
            read_pool_totals_at_or_before_snapshot(snapshot, previous)?,
            previous,
            "start-1 baseline",
        )?)
    };
    let end_orchard = end_totals.cumulative_orchard_value_balance_zat;
    let end_ironwood = end_totals.cumulative_ironwood_value_balance_zat;
    let base_orchard = baseline.map_or(0, |totals| totals.cumulative_orchard_value_balance_zat);
    let base_ironwood = baseline.map_or(0, |totals| totals.cumulative_ironwood_value_balance_zat);
    Ok(PoolDelta {
        orchard_outflow_zat: saturating_positive_delta(end_orchard, base_orchard),
        ironwood_inflow_zat: saturating_negative_magnitude(end_ironwood, base_ironwood),
    })
}

fn require_exact_pool_totals(
    totals: Option<MigrationPoolTotals>,
    expected_height: u32,
    role: &'static str,
) -> Result<MigrationPoolTotals, Status> {
    let totals = totals.ok_or_else(|| {
        ExplorerError::not_materialized(format!(
            "Ironwood Migration {role} pool-total record at height {expected_height} is unavailable",
        ))
    })?;
    if totals.block_height != expected_height {
        return Err(ExplorerError::not_materialized(format!(
            "Ironwood Migration {role} pool-total record expected height {expected_height}, found {}",
            totals.block_height,
        ))
        .into());
    }
    Ok(totals)
}

/// Magnitude of `end - base` when the range's net movement is positive
/// (value left the pool), zero otherwise.
fn saturating_positive_delta(end: i64, base: i64) -> u64 {
    u64::try_from(end.saturating_sub(base)).unwrap_or(0)
}

/// Magnitude of `end - base` when the range's net movement is negative
/// (value entered the pool), zero otherwise.
fn saturating_negative_magnitude(end: i64, base: i64) -> u64 {
    end.saturating_sub(base)
        .checked_neg()
        .and_then(|negated_delta| u64::try_from(negated_delta).ok())
        .unwrap_or(0)
}

fn read_migrations_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    start_height: u32,
    end_height: u32,
) -> Result<Vec<Migration>, Status> {
    let migrations = IronwoodMigrationConsumer::read_migrations_in_range_snapshot(
        snapshot,
        BlockHeight::new(start_height),
        BlockHeight::new(end_height),
        MAX_MIGRATION_ROWS_PER_REQUEST.saturating_add(1),
    )
    .map_err(|error| ExplorerError::internal(error.to_string()))?;
    require_migration_row_limit(migrations)
}

/// Rejects a range that cannot be aggregated exactly within the response cap.
///
/// The store scan fetches one sentinel row beyond the cap so a truncating
/// range iterator cannot turn an incomplete Overview, Cohorts, or
/// Denominations aggregate into a successful response.
fn require_migration_row_limit(migrations: Vec<Migration>) -> Result<Vec<Migration>, Status> {
    if migrations.len() > MAX_MIGRATION_ROWS_PER_REQUEST {
        return Err(Status::resource_exhausted(format!(
            "requested migration range exceeds the exact per-request cap of {MAX_MIGRATION_ROWS_PER_REQUEST} rows",
        )));
    }
    Ok(migrations)
}

fn read_pool_totals_at_or_before_snapshot(
    snapshot: &MaterializedViewStoreReadSnapshot<'_>,
    height: u32,
) -> Result<Option<MigrationPoolTotals>, Status> {
    IronwoodMigrationConsumer::read_pool_totals_at_or_before_snapshot(
        snapshot,
        BlockHeight::new(height),
    )
    .map_err(|error| ExplorerError::internal(error.to_string()).into())
}

fn validate_span(start_height: u32, end_height: u32) -> Result<(), Status> {
    if end_height < start_height {
        return Err(ExplorerError::invalid_request("end_height must be >= start_height").into());
    }
    let span = u64::from(end_height) - u64::from(start_height) + 1;
    if span > u64::from(MAX_MIGRATION_BLOCK_SPAN) {
        return Err(ExplorerError::invalid_request(format!(
            "requested span {span} blocks exceeds the per-request cap of {MAX_MIGRATION_BLOCK_SPAN}",
        ))
        .into());
    }
    Ok(())
}

#[allow(
    clippy::significant_drop_tightening,
    reason = "one borrowed snapshot must span both migration state and checkpoint validation"
)]
fn require_migration_snapshot_coherence(
    pinned: &WalletPinnedBlockSummarySnapshot<'_>,
) -> Result<(), Status> {
    let snapshot = pinned.snapshot();
    let migration_state = snapshot
        .consumer_state(IRONWOOD_MIGRATION_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(
                "Ironwood Migration materialized-view state is unavailable",
            )
        })?;
    require_matching_migration_state(migration_state, pinned.block_summary_state())?;
    let migration_checkpoint = snapshot
        .chain_event_checkpoint(IRONWOOD_MIGRATION_CONSUMER_NAME)
        .map_err(|error| ExplorerError::internal(error.to_string()))?
        .ok_or_else(|| {
            ExplorerError::not_materialized(
                "Ironwood Migration chain-event checkpoint is unavailable",
            )
        })?;
    if migration_checkpoint != pinned.block_summary_checkpoint() {
        return Err(ExplorerError::not_materialized(
            "Ironwood Migration checkpoint does not match the Block Summary snapshot",
        )
        .into());
    }
    Ok(())
}

fn require_matching_migration_state(
    migration_state: MaterializedViewState,
    block_summary_state: MaterializedViewState,
) -> Result<(), Status> {
    if migration_state != block_summary_state {
        return Err(ExplorerError::not_materialized(
            "Ironwood Migration state does not match the Block Summary snapshot",
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(
        missing_docs,
        reason = "Unit test names describe the behavior under test."
    )]

    use super::*;
    use prost::Message as _;
    use tempfile::tempdir;
    use zinder_core::{
        BlockHash, ChainEpochId, TransactionId,
        wire::{encode_height_key_ascending, encode_rpc_block_hash_hex},
    };
    use zinder_materialized_views::{
        BLOCK_SUMMARY_COLUMN_FAMILY, BLOCK_SUMMARY_CONSUMER_NAME, BLOCK_SUMMARY_SCHEMA,
        BlockSummaryConsumer, IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY,
        IRONWOOD_MIGRATION_SCHEMA, IRONWOOD_MIGRATIONS_COLUMN_FAMILY, MaterializedViewStoreOptions,
    };
    use zinder_proto::v1::explorer::{BlockSummary, BlockSummaryRecord};
    use zinder_proto::v1::wallet::{MaterializedViewHealth, MaterializedViewStatus};
    use zinder_store::{CanonicalEventCursor, RocksDbResourceBudget};

    fn migration(
        block_height: u32,
        tx_index_in_block: u32,
        anchor_byte: u8,
        migrated_amount_zat: u64,
        conformant: bool,
    ) -> Migration {
        let ironwood_value_balance_zat = -(i64::try_from(migrated_amount_zat).unwrap_or(i64::MAX));
        Migration {
            block_height,
            tx_index_in_block,
            transaction_id: TransactionId::from_bytes([anchor_byte; 32]),
            orchard_value_balance_zat: i64::try_from(migrated_amount_zat).unwrap_or(i64::MAX),
            ironwood_value_balance_zat,
            orchard_anchor: [anchor_byte; 32],
            conformant,
        }
    }

    #[test]
    fn denomination_floor_bins_by_power_of_ten() {
        assert_eq!(denomination_floor(0), 0);
        assert_eq!(denomination_floor(1), 1);
        assert_eq!(denomination_floor(9), 1);
        assert_eq!(denomination_floor(10), 10);
        assert_eq!(denomination_floor(999), 100);
        assert_eq!(denomination_floor(1_000), 1_000);
        assert_eq!(
            denomination_floor(2_100_000_000_000_000),
            1_000_000_000_000_000
        );
    }

    #[test]
    fn aggregate_overview_sums_amounts_and_tracks_boundary_heights() {
        let migrations = vec![
            migration(100, 0, 1, 500, true),
            migration(100, 3, 1, 1_500, false),
            migration(140, 1, 2, 4_000, true),
        ];
        let aggregate = aggregate_overview(&migrations);
        assert_eq!(aggregate.total_migrated_ironwood_zat, 6_000);
        assert_eq!(aggregate.migration_count, 3);
        assert_eq!(aggregate.first_height, Some(100));
        assert_eq!(aggregate.last_height, Some(140));
    }

    #[test]
    fn aggregate_overview_reports_none_boundaries_when_empty() {
        let aggregate = aggregate_overview(&[]);
        assert_eq!(aggregate.total_migrated_ironwood_zat, 0);
        assert_eq!(aggregate.migration_count, 0);
        assert_eq!(aggregate.first_height, None);
        assert_eq!(aggregate.last_height, None);
    }

    #[test]
    fn group_cohorts_groups_by_anchor_and_counts_conformant_members() {
        let migrations = vec![
            migration(100, 0, 1, 1_000, true),
            migration(101, 0, 1, 2_000, false),
            migration(102, 0, 2, 3_000, true),
        ];
        let (cohorts, stats) = group_cohorts(&migrations);
        assert_eq!(cohorts.len(), 2);

        let first = &cohorts[0];
        assert_eq!(first.orchard_anchor, vec![1u8; 32]);
        assert_eq!(first.member_count, 2);
        assert_eq!(first.conformant_member_count, 1);
        assert_eq!(first.total_migrated_zat, 3_000);

        let second = &cohorts[1];
        assert_eq!(second.orchard_anchor, vec![2u8; 32]);
        assert_eq!(second.member_count, 1);
        assert_eq!(second.conformant_member_count, 1);

        assert_eq!(stats.cohorts, 2);
        assert_eq!(stats.min_members, 1);
        assert_eq!(stats.max_members, 2);
        assert_eq!(stats.avg_members, 1);
    }

    #[test]
    fn group_cohorts_reports_zeroed_stats_when_empty() {
        let (cohorts, stats) = group_cohorts(&[]);
        assert!(cohorts.is_empty());
        assert_eq!(stats.cohorts, 0);
        assert_eq!(stats.avg_members, 0);
        assert_eq!(stats.min_members, 0);
        assert_eq!(stats.max_members, 0);
    }

    #[test]
    fn pool_delta_reports_outflow_only_when_orchard_net_leaves() {
        assert_eq!(saturating_positive_delta(100, 40), 60);
        assert_eq!(saturating_positive_delta(40, 100), 0);
        assert_eq!(saturating_positive_delta(50, 50), 0);
    }

    #[test]
    fn pool_delta_reports_inflow_only_when_ironwood_net_enters() {
        // A migration-heavy range: cumulative Ironwood balance falls (more
        // entered than left), so this must read as inflow, not zero.
        assert_eq!(saturating_negative_magnitude(-500, -100), 400);
        // A range where ordinary Ironwood spends dominate migrations: the
        // cumulative balance rises net, so inflow must clamp to zero rather
        // than reporting that rise as if it were inflow.
        assert_eq!(saturating_negative_magnitude(100, 40), 0);
        assert_eq!(saturating_negative_magnitude(50, 50), 0);
    }

    #[test]
    fn saturating_negative_magnitude_does_not_panic_at_i64_min() {
        assert_eq!(saturating_negative_magnitude(i64::MIN, 0), 0);
    }

    #[test]
    fn pool_delta_refuses_a_missing_exact_end_total() -> Result<(), &'static str> {
        let error = require_exact_pool_totals(None, 42, "end")
            .err()
            .ok_or("missing end total must fail")?;

        assert_eq!(error.code(), tonic::Code::NotFound);
        Ok(())
    }

    #[test]
    fn pool_delta_refuses_an_earlier_total_as_a_start_baseline() -> Result<(), &'static str> {
        let totals = MigrationPoolTotals {
            block_height: 40,
            cumulative_orchard_value_balance_zat: 0,
            cumulative_ironwood_value_balance_zat: 0,
            block_orchard_value_balance_zat: 0,
            block_ironwood_value_balance_zat: 0,
        };
        let error = require_exact_pool_totals(Some(totals), 41, "start-1 baseline")
            .err()
            .ok_or("earlier baseline total must fail")?;

        assert_eq!(error.code(), tonic::Code::NotFound);
        Ok(())
    }

    #[test]
    fn migration_row_cap_rejects_a_sentinel_row_instead_of_truncating() -> Result<(), &'static str>
    {
        let rows = std::iter::repeat_n(
            migration(100, 0, 1, 1, true),
            MAX_MIGRATION_ROWS_PER_REQUEST + 1,
        )
        .collect();
        let error = require_migration_row_limit(rows)
            .err()
            .ok_or("a sentinel migration row must reject the range")?;
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        Ok(())
    }

    #[test]
    fn bin_denominations_counts_only_conformant_rows() {
        let migrations = vec![
            migration(100, 0, 1, 500, true),
            migration(100, 1, 1, 700, true),
            migration(101, 0, 1, 4_000, true),
            migration(101, 1, 1, 9_000, false),
        ];
        let (bins, total_tx) = bin_denominations(&migrations);
        assert_eq!(total_tx, 3);
        assert_eq!(bins.len(), 2);
        assert_eq!(bins[0].denomination_zat, 100);
        assert_eq!(bins[0].count, 2);
        assert_eq!(bins[1].denomination_zat, 1_000);
        assert_eq!(bins[1].count, 1);
    }

    fn migration_key(height: u32, tx_index_in_block: u32) -> [u8; 8] {
        let mut key = [0u8; 8];
        key[..4].copy_from_slice(&encode_height_key_ascending(BlockHeight::new(height)));
        key[4..].copy_from_slice(&tx_index_in_block.to_be_bytes());
        key
    }

    fn migration_payload(record: Migration) -> [u8; 81] {
        let mut payload = [0u8; 81];
        payload[..32].copy_from_slice(&record.transaction_id.as_bytes());
        payload[32..40].copy_from_slice(&record.orchard_value_balance_zat.to_be_bytes());
        payload[40..48].copy_from_slice(&record.ironwood_value_balance_zat.to_be_bytes());
        payload[48..80].copy_from_slice(&record.orchard_anchor);
        payload[80] = u8::from(record.conformant);
        payload
    }

    fn pool_totals_payload(
        cumulative_orchard_value_balance_zat: i64,
        cumulative_ironwood_value_balance_zat: i64,
        block_orchard_value_balance_zat: i64,
        block_ironwood_value_balance_zat: i64,
    ) -> [u8; 32] {
        let mut payload = [0u8; 32];
        payload[..8].copy_from_slice(&cumulative_orchard_value_balance_zat.to_be_bytes());
        payload[8..16].copy_from_slice(&cumulative_ironwood_value_balance_zat.to_be_bytes());
        payload[16..24].copy_from_slice(&block_orchard_value_balance_zat.to_be_bytes());
        payload[24..].copy_from_slice(&block_ironwood_value_balance_zat.to_be_bytes());
        payload
    }

    fn snapshot_state(revision: u64, hash_seed: u8) -> MaterializedViewState {
        MaterializedViewState {
            chain_epoch_id: ChainEpochId::new(revision),
            tip_height: BlockHeight::new(100),
            tip_hash: BlockHash::from_bytes([hash_seed; 32]),
            revision,
            coverage: None,
        }
    }

    #[test]
    #[allow(
        clippy::significant_drop_tightening,
        clippy::too_many_lines,
        reason = "the E1 snapshot intentionally spans the E2 replacement and all three migration response-shape assertions"
    )]
    fn migration_snapshot_retains_e1_overview_cohorts_denominations_and_freshness_after_e2_write()
    -> eyre::Result<()> {
        let directory = tempdir()?;
        let activations = zinder_testkit::sample_regtest_upgrade_activations();
        let chain = zinder_testkit::ChainFixture::new(activations.network()).extend_blocks(2);
        let mut canonical_fixture =
            zinder_testkit::WalletServingStoreFixture::from_chain_after_live_append(
                &chain,
                &activations,
            )?;
        let identity = canonical_fixture.canonical_construction_identity()?;
        let (canonical_reader, _) = canonical_fixture.take_readers()?;
        let e1_checkpoint =
            zinder_materialized_views::MaterializedViewChainEventCheckpoint::from_retained_event(
                canonical_reader.retained_event_at_cursor(CanonicalEventCursor::at(1)?)?,
            );
        let e2_checkpoint =
            zinder_materialized_views::MaterializedViewChainEventCheckpoint::from_retained_event(
                canonical_reader.retained_event_at_cursor(CanonicalEventCursor::at(2)?)?,
            );
        let store = MaterializedViewStore::open(
            directory.path(),
            identity,
            MaterializedViewStoreOptions {
                sync_writes: false,
                consumers: &[IRONWOOD_MIGRATION_SCHEMA, BLOCK_SUMMARY_SCHEMA],
                rocksdb_resource_budget: RocksDbResourceBudget::for_local_tests(),
            },
        )?;
        let e1_state = snapshot_state(1, 0x11);
        let e1_migration = migration(100, 0, 1, 1_000, true);
        let e1_status = MaterializedViewStatus {
            health: MaterializedViewHealth::Live as i32,
            indexed_height: 100,
            lag_blocks: 0,
            observed_at_millis: 1_000,
        };
        store.put_consumer(
            IRONWOOD_MIGRATIONS_COLUMN_FAMILY,
            &migration_key(100, 0),
            &migration_payload(e1_migration),
        )?;
        store.put_consumer(
            IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(100)),
            &pool_totals_payload(1_000, -1_000, 1_000, -1_000),
        )?;
        store.put_consumer(
            BLOCK_SUMMARY_COLUMN_FAMILY,
            &BlockSummaryConsumer::key_for_height(e1_state.tip_height),
            &BlockSummaryRecord {
                summary: Some(BlockSummary {
                    block_height: 100,
                    block_hash: encode_rpc_block_hash_hex(e1_state.tip_hash),
                    ..Default::default()
                }),
                ..Default::default()
            }
            .encode_to_vec(),
        )?;
        store.put_consumer_state(IRONWOOD_MIGRATION_CONSUMER_NAME, e1_state)?;
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, e1_state)?;
        store.put_chain_event_checkpoint(IRONWOOD_MIGRATION_CONSUMER_NAME, e1_checkpoint)?;
        store.put_chain_event_checkpoint(BLOCK_SUMMARY_CONSUMER_NAME, e1_checkpoint)?;
        store.put_materialized_view_status(&e1_status.encode_to_vec())?;

        let e1_snapshot = store.read_snapshot()?;

        let e2_migration = migration(100, 1, 2, 9_000, true);
        store.put_consumer(
            IRONWOOD_MIGRATIONS_COLUMN_FAMILY,
            &migration_key(100, 1),
            &migration_payload(e2_migration),
        )?;
        store.put_consumer(
            IRONWOOD_MIGRATION_POOL_TOTALS_COLUMN_FAMILY,
            &encode_height_key_ascending(BlockHeight::new(100)),
            &pool_totals_payload(10_000, -10_000, 10_000, -10_000),
        )?;
        let e2_state = snapshot_state(2, 0x22);
        store.put_consumer_state(IRONWOOD_MIGRATION_CONSUMER_NAME, e2_state)?;
        store.put_consumer_state(BLOCK_SUMMARY_CONSUMER_NAME, e2_state)?;
        store.put_chain_event_checkpoint(IRONWOOD_MIGRATION_CONSUMER_NAME, e2_checkpoint)?;
        store.put_chain_event_checkpoint(BLOCK_SUMMARY_CONSUMER_NAME, e2_checkpoint)?;
        store.put_materialized_view_status(
            &MaterializedViewStatus {
                health: MaterializedViewHealth::Live as i32,
                indexed_height: 101,
                lag_blocks: 0,
                observed_at_millis: 1_001,
            }
            .encode_to_vec(),
        )?;

        let migrations = read_migrations_snapshot(&e1_snapshot, 100, 100)?;
        let overview = aggregate_overview(&migrations);
        let delta = read_range_pool_delta_snapshot(&e1_snapshot, 100, 100, 100)?;
        let (cohorts, cohort_stats) = group_cohorts(&migrations);
        let (denominations, total_tx) = bin_denominations(&migrations);
        let freshness = build_explorer_freshness_from_snapshot(
            &e1_snapshot,
            EXPLORER_MIGRATION_OVERVIEW_V1,
            None,
            0,
        )?;

        assert_eq!(overview.total_migrated_ironwood_zat, 1_000);
        assert_eq!(overview.migration_count, 1);
        assert_eq!(delta.orchard_outflow_zat, 1_000);
        assert_eq!(delta.ironwood_inflow_zat, 1_000);
        assert_eq!(cohorts.len(), 1);
        assert_eq!(cohorts[0].orchard_anchor, vec![1; 32]);
        assert_eq!(cohort_stats.cohorts, 1);
        assert_eq!(denominations.len(), 1);
        assert_eq!(denominations[0].denomination_zat, 1_000);
        assert_eq!(denominations[0].count, 1);
        assert_eq!(total_tx, 1);
        for consumer in [
            IRONWOOD_MIGRATION_CONSUMER_NAME,
            BLOCK_SUMMARY_CONSUMER_NAME,
        ] {
            assert_eq!(e1_snapshot.consumer_state(consumer)?, Some(e1_state));
            assert_eq!(
                e1_snapshot.chain_event_checkpoint(consumer)?,
                Some(e1_checkpoint)
            );
        }
        assert_eq!(
            freshness
                .chain_view
                .and_then(|chain_view| chain_view.materialized_views)
                .map(|status| status.observed_at_millis),
            Some(e1_status.observed_at_millis)
        );
        Ok(())
    }
}
